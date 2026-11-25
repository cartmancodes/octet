//! The runner, end to end: real git work trees, real run directories, and
//! the fake vendor (`protocol-child`) playing a fleet script.
//!
//! Each test has its own temporary directory: a git work tree `work/`, a data
//! directory `data/`, the fleet script and its turn log, and a wrapper script
//! that stands in for every vendor binary and names the script, so nothing
//! here changes the process environment and the tests can run in parallel.
//!
//! The runner and the test's event consumer run in one task (`join!`): the
//! consumer handles an event as soon as the runner next waits, before the
//! runner's next observation of the tree, which makes "an edit right after
//! the approval" deterministic.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use octet_core::ApprovalSummary;
use octet_fleet::definition::parse;
use octet_fleet::record::{
    Accepted, CandidateId, Decision, Failure, Origin, PauseReason, Record, Severity,
};
use octet_fleet::store::{self, RunDir};
use octet_fleet::validate::{Host, Refs, Resolved, validate};
use octet_fleet::{
    Approver, Clock, FleetEvent, RunEnd, RunnerError, RunnerIo, Start, resume, start, status, stop,
};
use octet_testkit::{TempDir, write_script};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

/// The workflow the repository ships.
const SHIPPED: &str = include_str!("../../../docs/fleet/research-plan-build.yaml");

/// A small workflow for the decision tests: one document and its review, a
/// change, its tests and its review, all on Codex, one round per edge.
const SMALL: &str = r#"
version: 1
name: small
limits: { max_rounds: 1, transient_retries: 3 }
profiles:
  doc: { engine: codex }
  code: { engine: codex, mode: full-access }
roles:
  researcher: { kind: document, instructions: Research the task. }
  reviewer: { kind: verdict, instructions: Review the artifact. }
  coder: { kind: change, instructions: Implement the research. }
  tester: { kind: tests, instructions: Test the change. }
steps:
  - { id: research, role: researcher, profile: doc }
  - { id: review_research, role: reviewer, profile: doc, reviews: research }
  - id: implement
    role: coder
    profile: code
    inputs: [{ from: research, approval_from: review_research }]
  - { id: test, role: tester, profile: code, change_from: implement, commands: ["true"] }
  - { id: review_change, role: reviewer, profile: doc, reviews: implement, evidence_from: test }
"#;

/// How long any one runner call may take.
const PATIENCE: Duration = Duration::from_secs(120);

/// A host whose every engine runs `vendor`.
struct FakeHost {
    vendor: PathBuf,
}

impl Host for FakeHost {
    fn engine_known(&self, _: &str) -> bool {
        true
    }
    fn check_effort(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn resolve_binary(&self, _: &str, _: Option<&Path>) -> Option<PathBuf> {
        Some(self.vendor.clone())
    }
}

/// An approver that allows everything.
struct Allow;

impl Approver for Allow {
    fn ask<'a>(
        &'a mut self,
        _summary: &'a ApprovalSummary,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async { true })
    }
}

/// A verdict reply: `approved` with no findings, resolving `resolved`.
fn approve(resolved: &[&str]) -> String {
    verdict("approved", &[], resolved)
}

/// A verdict reply with `new` blocking findings, resolving `resolved`.
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

/// The turns of a run where every step does its work and every review
/// approves: the coder writes `feature.txt`, the tester `feature_test.sh`.
fn happy_turns() -> Value {
    json!({
        "research#1": {"reply": "The repository holds one README."},
        "review_research#1": {"reply": approve(&[])},
        "plan#1": {"reply": "1. Add feature.txt."},
        "review_plan#1": {"reply": approve(&[])},
        "implement#1": {"write": {"feature.txt": "the feature\n"}, "reply": "Added feature.txt."},
        "test#1": {"write": {"feature_test.sh": "test -f feature.txt\n"}, "reply": "Added a test."},
        "review_change#1": {"reply": approve(&[])},
    })
}

/// One test's directories, work tree and fake vendor.
struct Bench {
    _dir: TempDir,
    work: PathBuf,
    data: PathBuf,
    script: PathBuf,
    turns: PathBuf,
    vendor: PathBuf,
}

impl Bench {
    fn new(turns: &Value) -> Self {
        let dir = TempDir::new("octet-fleet-runner");
        let work = dir.path().join("work");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "--quiet"]);
        git(&work, &["config", "user.name", "Tester"]);
        git(&work, &["config", "user.email", "tester@example.com"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("README"), "a repository\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "--quiet", "-m", "base"]);
        let script = dir.path().join("script.json");
        let turns_log = dir.path().join("turns.log");
        let vendor = write_script(
            &dir.path().join("bin"),
            "vendor",
            &format!(
                "OCTET_FLEET_SCRIPT='{}' exec '{}' \"$@\"",
                script.display(),
                octet_testkit::protocol_child().display()
            ),
        );
        let bench = Self {
            _dir: dir,
            work,
            data,
            script,
            turns: turns_log,
            vendor,
        };
        bench.script(turns);
        bench
    }

    /// Replaces the fleet script's turns.
    fn script(&self, turns: &Value) {
        let body = json!({"log": self.turns, "turns": turns});
        std::fs::write(&self.script, body.to_string()).unwrap();
    }

    /// `yaml`, validated against this bench's vendor, with every tests
    /// step's commands replaced by `commands`.
    fn workflow(&self, yaml: &str, commands: &[&str]) -> Resolved {
        let host = FakeHost {
            vendor: self.vendor.clone(),
        };
        let mut workflow = validate(&parse(yaml).unwrap(), &host).unwrap();
        for step in &mut workflow.steps {
            if let Refs::Tests { commands: c, .. } = &mut step.refs {
                *c = commands.iter().map(|&c| c.to_owned()).collect();
            }
        }
        workflow
    }

    fn start(&self, workflow: Resolved) -> Start {
        Start {
            workflow,
            hash: "0123456789abcdef".into(),
            task: "Add the feature.".into(),
            cwd: self.work.clone(),
            data_dir: self.data.clone(),
        }
    }

    /// The only run's ID.
    async fn run_id(&self) -> String {
        let runs = status(&self.data, None).await.unwrap();
        assert_eq!(runs.len(), 1, "{runs:?}");
        runs[0].id.clone()
    }

    async fn records(&self) -> Vec<Record> {
        let root = self.data.join("fleet").join(self.run_id().await);
        store::read_log(&RunDir { root }).await.unwrap()
    }

    /// The fake's turn log: one JSON object per played turn.
    fn played(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.turns)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// How many attempts of `step` the log started.
    async fn attempts_of(&self, step: &str) -> usize {
        self.records()
            .await
            .iter()
            .filter(|r| matches!(r, Record::AttemptStarted { step: s, .. } if s == step))
            .count()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(name);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The channels of one runner call.
struct Io {
    runner: RunnerIo,
    events: mpsc::Receiver<FleetEvent>,
    signal: Arc<watch::Sender<Option<&'static str>>>,
}

fn channels() -> Io {
    let (tx, events) = mpsc::channel(64);
    let (signal, signals) = watch::channel(None);
    Io {
        runner: RunnerIo::new(tx, Box::new(Allow), signals),
        events,
        signal: Arc::new(signal),
    }
}

/// Runs `run` with a consumer that calls `on` for every event; the end and
/// every event, in order.
async fn drive(
    run: impl Future<Output = Result<RunEnd, RunnerError>>,
    mut events: mpsc::Receiver<FleetEvent>,
    mut on: impl FnMut(&FleetEvent),
) -> (Result<RunEnd, RunnerError>, Vec<FleetEvent>) {
    let consume = async {
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            on(&event);
            seen.push(event);
        }
        seen
    };
    tokio::join!(run, consume)
}

/// [`drive`] under [`PATIENCE`].
async fn patiently(
    run: impl Future<Output = Result<RunEnd, RunnerError>>,
    events: mpsc::Receiver<FleetEvent>,
    on: impl FnMut(&FleetEvent),
) -> (RunEnd, Vec<FleetEvent>) {
    let (end, events) = tokio::time::timeout(PATIENCE, drive(run, events, on))
        .await
        .expect("the runner returned in time");
    (end.unwrap(), events)
}

/// Resumes the bench's run, taking `decision`, handling no events.
async fn resume_with(bench: &Bench, decision: Option<Decision>) -> (RunEnd, Vec<FleetEvent>) {
    let Io {
        runner: io, events, ..
    } = channels();
    let id = bench.run_id().await;
    patiently(resume(&bench.data, &id, decision, io), events, |_| {}).await
}

/// The `completed` record.
fn completed(records: &[Record]) -> (String, String) {
    records
        .iter()
        .find_map(|r| match r {
            Record::Completed {
                tree,
                observed_tree,
                ..
            } => Some((tree.clone(), observed_tree.clone())),
            _ => None,
        })
        .expect("a completed record")
}

#[tokio::test]
async fn a_full_run_completes() {
    let bench = Bench::new(&happy_turns());
    let workflow = bench.workflow(SHIPPED, &["test -f feature.txt", "sh feature_test.sh"]);
    let Io {
        runner: io, events, ..
    } = channels();
    // While the run is driven, `status` (no lock taken) says it is running.
    let during = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&during);
    let data = bench.data.clone();
    let (end, events) = patiently(start(bench.start(workflow), io), events, |event| {
        if let FleetEvent::AttemptStarted { step, .. } = event
            && step == "plan"
        {
            let (seen, data) = (Arc::clone(&seen), data.clone());
            tokio::spawn(async move {
                let runs = status(&data, None).await.unwrap();
                *seen.lock().unwrap() = Some(runs[0].state.clone());
            });
        }
    })
    .await;
    assert_eq!(end, RunEnd::Completed, "{events:#?}");
    assert_eq!(during.lock().unwrap().as_deref(), Some("running"));
    let records = bench.records().await;
    let (tree, observed_tree) = completed(&records);
    assert_eq!(observed_tree, tree);
    // Every step ran once, in order, and the evidence passed.
    let steps: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            Record::AttemptStarted { step, .. } => Some(step.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        [
            "research",
            "review_research",
            "plan",
            "review_plan",
            "implement",
            "test",
            "review_change"
        ]
    );
    assert!(records.iter().any(|r| matches!(
        r,
        Record::AttemptFinished {
            outcome: Accepted::TestsPassed { .. },
            ..
        }
    )));
    // The tester's file is a derived candidate the evidence ran on.
    assert!(records.iter().any(|r| matches!(
        r,
        Record::Candidate {
            origin: Origin::Tests,
            ..
        }
    )));
    assert!(bench.work.join("feature.txt").exists());
    let Some(FleetEvent::Completed { tree: shown, .. }) = events.last() else {
        panic!("{:?}", events.last());
    };
    assert_eq!(*shown, tree);
    // The reviewer of the plan read the research before the plan.
    let played = bench.played();
    let review_plan = played
        .iter()
        .find(|turn| turn["key"] == "review_plan#1")
        .unwrap();
    let prompt = review_plan["prompt"].as_str().unwrap();
    let research = prompt.find("## Approved document (research)").unwrap();
    let plan = prompt.find("## Document under review (plan)").unwrap();
    assert!(research < plan, "{prompt}");
    // Read-only steps ran in their clone; write steps in the work tree.
    let cwd_of = |key: &str| {
        let turn = played.iter().find(|turn| turn["key"] == key).unwrap();
        PathBuf::from(turn["cwd"].as_str().unwrap())
    };
    assert!(cwd_of("review_change#1").ends_with("tree"));
    assert_eq!(
        cwd_of("implement#1").canonicalize().unwrap(),
        bench.work.canonicalize().unwrap()
    );
    let summary = &status(&bench.data, None).await.unwrap()[0];
    assert_eq!(summary.state, "completed");
    assert_eq!(summary.step.as_deref(), Some("review_change"));
    // The run no longer owns the work tree: another run may start.
    let records_dir = bench.data.join("fleet").join("worktrees");
    for entry in std::fs::read_dir(records_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let record: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(record["runs"], json!({}));
        }
    }
}

#[tokio::test]
async fn a_reviewer_write_is_an_access_violation() {
    let mut turns = happy_turns();
    turns["review_research#1"] = json!([
        {"write": {"notes.txt": "the reviewer's notes\n"}, "reply": approve(&[])},
        {"reply": approve(&[])},
    ]);
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SHIPPED, &["test -f feature.txt"]);
    let Io {
        runner: io, events, ..
    } = channels();
    let (end, _) = patiently(start(bench.start(workflow), io), events, |_| {}).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::AccessViolation));
    let records = bench.records().await;
    let Some(Record::AttemptFailed {
        failure: Failure::AccessViolation { diff_stat },
        ..
    }) = records
        .iter()
        .find(|r| matches!(r, Record::AttemptFailed { .. }))
    else {
        panic!("{records:#?}");
    };
    assert!(diff_stat.contains("notes.txt"), "{diff_stat}");
    // The write landed in the clone, never in the work tree, and the verdict
    // was discarded.
    assert!(!bench.work.join("notes.txt").exists());
    assert!(!records.iter().any(|r| matches!(
        r,
        Record::AttemptFinished {
            outcome: Accepted::DocumentApproved { .. },
            ..
        }
    )));
    assert_eq!(status(&bench.data, None).await.unwrap()[0].state, "paused");
    // `retry` runs the review again in a fresh clone, and the run goes on.
    let (end, _) = resume_with(&bench, Some(Decision::Retry)).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(bench.attempts_of("review_research").await, 2);
}

/// An approval with one minor finding, `summary`, about `file`.
fn approve_with_minor(summary: &str, file: &str) -> String {
    let block = json!({
        "verdict": "approved",
        "new_findings": [{"severity": "minor", "summary": summary, "file": file}],
        "previous": [],
    });
    format!("Fine.\n\n```json\n{block}\n```\n")
}

#[tokio::test]
async fn open_minor_findings_are_reported_at_completion() {
    let mut turns = happy_turns();
    turns["review_research#1"] =
        json!({"reply": approve_with_minor("Cite the README.", "README.md")});
    turns["review_change#1"] =
        json!({"reply": approve_with_minor("feature.txt could say more.", "feature.txt")});
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SHIPPED, &["test -f feature.txt"]);
    let Io {
        runner: io, events, ..
    } = channels();
    let (end, events) = patiently(start(bench.start(workflow), io), events, |_| {}).await;
    assert_eq!(end, RunEnd::Completed, "{events:#?}");
    let Some(FleetEvent::Completed { open_findings, .. }) = events.last() else {
        panic!("{:?}", events.last());
    };
    let shown: Vec<(&str, Severity, &str, Option<&str>)> = open_findings
        .iter()
        .map(|f| {
            (
                f.id.as_str(),
                f.severity,
                f.summary.as_str(),
                f.file.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                "review_change/F1",
                Severity::Minor,
                "feature.txt could say more.",
                Some("feature.txt")
            ),
            (
                "review_research/F1",
                Severity::Minor,
                "Cite the README.",
                Some("README.md")
            ),
        ]
    );
}

#[tokio::test]
async fn an_edit_after_approval_is_drift() {
    let mut turns = happy_turns();
    turns["test#2"] = json!({"reply": "The tests stand."});
    turns["review_change#2"] = json!({"reply": approve(&[])});
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SHIPPED, &["test -f feature.txt"]);
    let Io {
        runner: io, events, ..
    } = channels();
    let late = bench.work.join("late.txt");
    let edit = late.clone();
    let (end, _) = patiently(start(bench.start(workflow), io), events, |event| {
        // Right after the final approval is durable, before the runner
        // looks at the tree again.
        if let FleetEvent::AttemptFinished { outcome, .. } = event
            && outcome == "change_approved"
        {
            std::fs::write(&edit, "edited after the approval\n").unwrap();
        }
    })
    .await;
    assert_eq!(end, RunEnd::Paused(PauseReason::Drift));
    let records = bench.records().await;
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, Record::Completed { .. }))
    );
    let Some(Record::Paused {
        reason: PauseReason::Drift,
        detail,
    }) = records.last()
    else {
        panic!("{:?}", records.last());
    };
    // The pause shows what changed: the diffstat and the command for the
    // whole diff, between the recorded tree and the live one.
    assert!(detail.contains("late.txt"), "{detail}");
    assert!(detail.contains("1 file changed"), "{detail}");
    let command = detail
        .lines()
        .find_map(|line| line.split_once("git diff ").map(|(_, trees)| trees))
        .unwrap_or_else(|| panic!("no git diff command: {detail}"));
    let trees: Vec<&str> = command.split_whitespace().collect();
    assert_eq!(trees.len(), 2, "{detail}");
    let shown = std::process::Command::new("git")
        .args(["diff", "--stat", trees[0], trees[1]])
        .current_dir(&bench.work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(shown.status.success(), "{shown:?}");
    assert!(String::from_utf8_lossy(&shown.stdout).contains("late.txt"));
    // `adopt` derives a candidate from the tip; the tests and the review
    // run again on it, the coder does not.
    let (end, _) = resume_with(
        &bench,
        Some(Decision::Adopt {
            candidate: CandidateId(0),
        }),
    )
    .await;
    assert_eq!(end, RunEnd::Completed);
    let records = bench.records().await;
    assert!(records.iter().any(|r| matches!(
        r,
        Record::Candidate {
            origin: Origin::Drift,
            ..
        }
    )));
    assert_eq!(bench.attempts_of("implement").await, 1);
    assert_eq!(bench.attempts_of("test").await, 2);
    assert_eq!(bench.attempts_of("review_change").await, 2);
    let (tree, observed_tree) = completed(&records);
    assert_eq!(observed_tree, tree);
    assert!(late.exists());
}

#[tokio::test]
async fn a_transient_retry_waits_out_its_backoff_across_a_restart() {
    let mut turns = happy_turns();
    turns["research#1"] = json!([{"fail": true}, {"reply": "The repository holds one README."}]);
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SHIPPED, &["test -f feature.txt"]);
    // A clock that follows tokio's, so paused time moves it.
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let anchor = tokio::time::Instant::now();
    let clock = Clock::new(move || epoch + anchor.elapsed().as_secs());

    // The first run fails transiently and is interrupted while it waits.
    let Io {
        runner: mut io,
        events,
        signal,
    } = channels();
    io.clock = clock.clone();
    let retrying = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&retrying);
    let (end, _) = patiently(start(bench.start(workflow), io), events, |event| {
        if let FleetEvent::Retrying { at, .. } = event {
            *seen.lock().unwrap() = Some(*at);
            signal.send(Some("SIGINT")).unwrap();
        }
    })
    .await;
    assert_eq!(end, RunEnd::Interrupted("SIGINT"));
    let at = retrying.lock().unwrap().expect("a retry was announced");
    let records = bench.records().await;
    let failed_at = clock.now();
    let Some(Record::Retry {
        count, eligible_at, ..
    }) = records.iter().find(|r| matches!(r, Record::Retry { .. }))
    else {
        panic!("{records:#?}");
    };
    assert_eq!((*count, *eligible_at), (1, at));
    assert!(
        at >= epoch + 60 && at <= failed_at + 60,
        "{at} vs {failed_at}"
    );
    assert_eq!(bench.attempts_of("research").await, 1);

    // The restart waits out the rest of the backoff before the retry.
    tokio::time::pause();
    let Io {
        runner: mut io,
        events,
        ..
    } = channels();
    io.clock = clock.clone();
    let started_at = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&started_at);
    let id = bench.run_id().await;
    let (end, _) = drive(resume(&bench.data, &id, None, io), events, |event| {
        if let FleetEvent::AttemptStarted { step, .. } = event
            && step == "research"
        {
            *seen.lock().unwrap() = Some(clock.now());
            // The wait is over: the vendors run on real time.
            tokio::time::resume();
        }
    })
    .await;
    assert_eq!(end.unwrap(), RunEnd::Completed);
    let started_at = started_at.lock().unwrap().expect("the retry started");
    assert!(
        started_at >= at,
        "the retry started at {started_at}, before {at}"
    );
    assert_eq!(bench.attempts_of("research").await, 2);
}

#[tokio::test]
async fn a_transient_failure_that_changed_the_tree_keeps_its_backoff() {
    let mut turns = small_turns();
    turns["implement#1"] = json!([
        {"write": {"partial.txt": "half done\n"}, "fail": true},
        {"write": {"feature.txt": "done\n"}, "reply": "Finished."},
    ]);
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SMALL, &["test -f feature.txt"]);
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let anchor = tokio::time::Instant::now();
    let clock = Clock::new(move || epoch + anchor.elapsed().as_secs());
    let Io {
        runner: mut io,
        events,
        ..
    } = channels();
    io.clock = clock.clone();
    let (end, _) = patiently(start(bench.start(workflow), io), events, |_| {}).await;
    // The coder edited before the usage limit: the tree is reconciled, and
    // the backoff is logged all the same.
    assert_eq!(end, RunEnd::Paused(PauseReason::Reconcile));
    let records = bench.records().await;
    let at = records
        .iter()
        .find_map(|r| match r {
            Record::AttemptFailed {
                failure: Failure::Transient { eligible_at, .. },
                ..
            } => *eligible_at,
            _ => None,
        })
        .expect("the failure carries its retry time");
    assert!(
        records.iter().any(
            |r| matches!(r, Record::Retry { count: 1, eligible_at, .. } if *eligible_at == at)
        )
    );
    assert!(at >= epoch + 60, "{at}");

    // `keep` continues the coder only once the backoff is over.
    tokio::time::pause();
    let Io {
        runner: mut io,
        events,
        ..
    } = channels();
    io.clock = clock.clone();
    let started_at = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&started_at);
    let id = bench.run_id().await;
    let (end, _) = drive(
        resume(&bench.data, &id, Some(Decision::Keep), io),
        events,
        |event| {
            if let FleetEvent::AttemptStarted { step, .. } = event
                && step == "implement"
            {
                *seen.lock().unwrap() = Some(clock.now());
                tokio::time::resume();
            }
        },
    )
    .await;
    assert_eq!(end.unwrap(), RunEnd::Completed);
    let started_at = started_at
        .lock()
        .unwrap()
        .expect("the kept attempt started");
    assert!(started_at >= at, "kept at {started_at}, before {at}");
    assert_eq!(bench.attempts_of("implement").await, 2);
}

#[tokio::test]
async fn decisions_apply() {
    reconcile_keep().await;
    reconcile_adopt().await;
    drift_adopt().await;
    round_limit_continue().await;
}

/// Turns for [`SMALL`] where every step does its work.
fn small_turns() -> Value {
    json!({
        "research#1": {"reply": "The repository holds one README."},
        "review_research#1": {"reply": approve(&[])},
        "implement#1": {"write": {"feature.txt": "the feature\n"}, "reply": "Added feature.txt."},
        "test#1": {"reply": "The command covers it."},
        "review_change#1": {"reply": approve(&[])},
    })
}

/// A run of [`SMALL`] whose coder is interrupted after writing
/// `partial.txt`: paused for `Reconcile` on resume. The coder's second play
/// is `then`.
async fn reconciling(then: Value) -> Bench {
    let mut turns = small_turns();
    turns["implement#1"] = json!([
        {"write": {"partial.txt": "half done\n"}, "hold": "after_write"},
        then,
    ]);
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SMALL, &["test -f partial.txt"]);
    let Io {
        runner: io,
        events,
        signal,
    } = channels();
    let partial = bench.work.join("partial.txt");
    let (end, _) = patiently(start(bench.start(workflow), io), events, |event| {
        if let FleetEvent::AttemptStarted { step, .. } = event
            && step == "implement"
        {
            // Interrupt once the coder has written, while it holds its turn.
            let (signal, partial) = (Arc::clone(&signal), partial.clone());
            tokio::spawn(async move {
                while !partial.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                signal.send(Some("SIGINT")).unwrap();
            });
        }
    })
    .await;
    assert_eq!(end, RunEnd::Interrupted("SIGINT"));
    let records = bench.records().await;
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Record::AttemptInterrupted { .. }))
    );
    // An interrupt needs no decision: resume finds the coder's edit.
    let (end, _) = resume_with(&bench, None).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::Reconcile));
    // A decision the pause does not offer is refused, and nothing is logged.
    let before = bench.records().await.len();
    let Io {
        runner: io, events, ..
    } = channels();
    let id = bench.run_id().await;
    let (refused, _) = drive(
        resume(&bench.data, &id, Some(Decision::Continue), io),
        events,
        |_| {},
    )
    .await;
    assert!(
        matches!(refused, Err(RunnerError::Refused(_))),
        "{refused:?}"
    );
    assert_eq!(bench.records().await.len(), before);
    // A signal before anything runs leaves the pause, and nothing else, in
    // the log: the decision is not taken and no interrupt pause hides it.
    let Io {
        runner: io,
        events,
        signal,
    } = channels();
    signal.send(Some("SIGINT")).unwrap();
    let (end, _) = patiently(
        resume(&bench.data, &id, Some(Decision::Keep), io),
        events,
        |_| {},
    )
    .await;
    assert_eq!(end, RunEnd::Interrupted("SIGINT"));
    assert_eq!(bench.records().await.len(), before);
    bench
}

async fn reconcile_keep() {
    let bench =
        reconciling(json!({"write": {"feature.txt": "done\n"}, "reply": "Finished."})).await;
    let (end, _) = resume_with(&bench, Some(Decision::Keep)).await;
    assert_eq!(end, RunEnd::Completed);
    // The kept attempt continued the vendor session, told why.
    let played = bench.played();
    let kept = played
        .iter()
        .filter(|turn| turn["key"] == "implement#1")
        .nth(1)
        .unwrap();
    assert_eq!(kept["resumed"], true);
    assert!(
        kept["prompt"]
            .as_str()
            .unwrap()
            .contains("continues one that was interrupted")
    );
    assert_eq!(bench.attempts_of("implement").await, 2);
    assert!(bench.work.join("partial.txt").exists());
    assert!(bench.work.join("feature.txt").exists());
}

async fn reconcile_adopt() {
    let bench = reconciling(json!({"reply": "never played"})).await;
    let (end, _) = resume_with(
        &bench,
        Some(Decision::Adopt {
            candidate: CandidateId(0),
        }),
    )
    .await;
    assert_eq!(end, RunEnd::Completed);
    // The interrupted attempt's tree became its change: the coder never ran again.
    let records = bench.records().await;
    let adopted = records
        .iter()
        .find_map(|r| match r {
            Record::Decision {
                decision: Decision::Adopt { candidate },
            } => Some(*candidate),
            _ => None,
        })
        .unwrap();
    assert!(records.iter().any(|r| matches!(
        r,
        Record::Candidate { id, origin: Origin::Change, .. } if *id == adopted
    )));
    assert!(records.iter().any(|r| matches!(
        r,
        Record::AttemptFinished {
            outcome: Accepted::ChangeProduced { .. },
            candidate: Some(k),
            ..
        } if *k == adopted
    )));
    assert_eq!(bench.attempts_of("implement").await, 1);
}

async fn drift_adopt() {
    let bench = Bench::new(&small_turns());
    let workflow = bench.workflow(SMALL, &["test -f drift.txt"]);
    let Io {
        runner: io,
        events,
        signal,
    } = channels();
    // Interrupted as the research starts: its vendor is never released.
    let (end, _) = patiently(start(bench.start(workflow), io), events, |event| {
        if let FleetEvent::AttemptStarted { step, .. } = event
            && step == "research"
        {
            signal.send(Some("SIGTERM")).unwrap();
        }
    })
    .await;
    assert_eq!(end, RunEnd::Interrupted("SIGTERM"));
    assert!(
        bench.played().is_empty(),
        "the vendor ran: {:?}",
        bench.played()
    );
    // Edited while the run was interrupted: resume finds drift.
    std::fs::write(bench.work.join("drift.txt"), "edited by hand\n").unwrap();
    let (end, events) = resume_with(&bench, None).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::Drift));
    assert!(events.iter().any(|e| matches!(
        e,
        FleetEvent::NeedsDecision { reason: PauseReason::Drift, options } if options == &["adopt", "stop"]
    )));
    let (end, _) = resume_with(
        &bench,
        Some(Decision::Adopt {
            candidate: CandidateId(0),
        }),
    )
    .await;
    assert_eq!(end, RunEnd::Completed);
    let records = bench.records().await;
    assert!(records.iter().any(|r| matches!(
        r,
        Record::Candidate {
            origin: Origin::Drift,
            parent: Some(CandidateId(0)),
            ..
        }
    )));
    let (tree, observed_tree) = completed(&records);
    assert_eq!(observed_tree, tree);
}

async fn round_limit_continue() {
    let mut turns = small_turns();
    turns["review_research#1"] =
        json!({"reply": verdict("changes_requested", &["Too thin."], &[])});
    turns["research#2"] = json!({"reply": "The repository holds one README, read twice."});
    turns["review_research#2"] = json!({
        "reply": verdict("changes_requested", &["Still thin."], &["review_research/F1"])
    });
    turns["research#3"] = json!({"reply": "The repository holds one README, in full."});
    turns["review_research#3"] = json!({"reply": approve(&["review_research/F2"])});
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SMALL, &["true"]);
    let Io {
        runner: io, events, ..
    } = channels();
    let (end, _) = patiently(start(bench.start(workflow), io), events, |_| {}).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::RoundLimit));
    assert_eq!(bench.attempts_of("research").await, 2);
    // The research revised after the first rejection carried its finding.
    let played = bench.played();
    let second = played.iter().find(|t| t["key"] == "research#2").unwrap();
    assert!(
        second["prompt"]
            .as_str()
            .unwrap()
            .contains("review_research/F1")
    );
    let (end, _) = resume_with(&bench, Some(Decision::Continue)).await;
    assert_eq!(end, RunEnd::Completed);
    assert_eq!(bench.attempts_of("research").await, 3);
}

/// The runs the work tree's ownership record lists.
fn listed(data: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data.join("fleet").join("worktrees")) else {
        return Vec::new();
    };
    let mut runs = Vec::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            let record: Value =
                serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
            runs.extend(record["runs"].as_object().unwrap().keys().cloned());
        }
    }
    runs
}

#[tokio::test]
async fn a_run_paused_between_attempts_holds_no_claim_on_the_work_tree() {
    // The tests fail; sent back, the coder changes nothing: a stall, with
    // every write attempt finished.
    let mut turns = small_turns();
    turns["implement#2"] = json!({"reply": "Nothing to change."});
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SMALL, &["test -f absent.txt"]);
    let Io {
        runner: io, events, ..
    } = channels();
    let (end, _) = patiently(start(bench.start(workflow.clone()), io), events, |_| {}).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::Stalled));
    assert!(bench.attempts_of("implement").await >= 2);
    assert_eq!(listed(&bench.data), Vec::<String>::new());
    // So another run may write the work tree.
    let Io {
        runner: io, events, ..
    } = channels();
    let (end, _) = patiently(start(bench.start(workflow), io), events, |_| {}).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::Stalled));
}

#[tokio::test]
async fn an_unfinished_write_attempt_keeps_its_claim_on_the_work_tree() {
    let bench = reconciling(json!({"reply": "Finished."})).await;
    assert_eq!(listed(&bench.data), [bench.run_id().await]);
}

/// A run of [`SMALL`] interrupted while its researcher holds its turn; its
/// bench and the interrupted attempt's clone.
async fn research_interrupted() -> (Bench, PathBuf) {
    let mut turns = small_turns();
    turns["research#1"] = json!([
        {"hold": "before_write"},
        {"reply": "The repository holds one README."},
    ]);
    let bench = Bench::new(&turns);
    let workflow = bench.workflow(SMALL, &["test -f feature.txt"]);
    let Io {
        runner: io,
        events,
        signal,
    } = channels();
    let log = bench.turns.clone();
    let (end, _) = patiently(start(bench.start(workflow), io), events, |event| {
        if matches!(event, FleetEvent::AttemptStarted { step, .. } if step == "research") {
            // Interrupt once the researcher holds its turn.
            let (signal, log) = (Arc::clone(&signal), log.clone());
            tokio::spawn(async move {
                while !std::fs::read_to_string(&log).is_ok_and(|text| text.contains("research#1")) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                signal.send(Some("SIGINT")).unwrap();
            });
        }
    })
    .await;
    assert_eq!(end, RunEnd::Interrupted("SIGINT"));
    let id = bench.run_id().await;
    let clone = bench
        .data
        .join("fleet")
        .join(&id)
        .join("attempts")
        .join("0")
        .join("tree");
    assert!(clone.exists());
    (bench, clone)
}

#[tokio::test]
async fn a_new_attempt_discards_the_clone_of_an_interrupted_one() {
    let (bench, clone) = research_interrupted().await;
    let (end, _) = resume_with(&bench, None).await;
    assert_eq!(end, RunEnd::Completed);
    assert!(!clone.exists(), "the interrupted attempt's clone is kept");
}

#[tokio::test]
async fn stop_discards_the_clone_of_an_interrupted_read_only_attempt() {
    let (bench, clone) = research_interrupted().await;
    assert!(clone.exists());
    let id = bench.run_id().await;
    assert_eq!(stop(&bench.data, &id).await.unwrap(), RunEnd::Stopped);
    assert!(!clone.exists(), "a stopped run's clone is kept");
}

#[tokio::test]
async fn a_clone_that_cannot_be_discarded_pauses_the_run() {
    let (bench, clone) = research_interrupted().await;
    // Something that is not a directory stands where the clone was.
    std::fs::remove_dir_all(&clone).unwrap();
    std::fs::write(&clone, "not a clone").unwrap();
    let before = bench.attempts_of("research").await;
    let (end, _) = resume_with(&bench, None).await;
    assert_eq!(end, RunEnd::Paused(PauseReason::LocalFailure));
    assert_eq!(bench.attempts_of("research").await, before);
    let records = bench.records().await;
    assert!(
        matches!(
            records.last(),
            Some(Record::Paused { reason: PauseReason::LocalFailure, detail })
                if detail.contains(&clone.display().to_string())
        ),
        "{records:#?}"
    );
    // Once it can be, `retry` discards it and the run goes on.
    std::fs::remove_file(&clone).unwrap();
    let (end, _) = resume_with(&bench, Some(Decision::Retry)).await;
    assert_eq!(end, RunEnd::Completed);
}
