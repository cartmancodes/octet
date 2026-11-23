//! One attempt of one step against a real session on the fake vendor
//! (`protocol-child`), with a real run directory and log.
//!
//! The fake reads its fleet script from `OCTET_FLEET_SCRIPT`. Each test sets
//! it in a wrapper script that stands in for the vendor binary, so nothing
//! here changes the process environment and the tests can run in parallel.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use octet_core::{ApprovalSummary, Event};
use octet_fleet::{
    FleetEvent,
    classify::FailureClass,
    definition::{Kind, Limits, SessionPolicy},
    machine::Dispatch,
    record::{AttemptId, CandidateId, Record},
    step::{Approver, AttemptPlan, StepError, TurnEnd, run_attempt},
    store::{self, Log, RunDir, RunFile},
    validate::{Refs, Resolved, ResolvedProfile, Step},
};
use octet_testkit::{TempDir, scenario, write_script};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// The run ID every test uses; each has its own data directory.
const RUN: &str = "f0000000001-abcd";

/// A run directory, its log, a work tree and a fake vendor whose fleet
/// script holds `turns`.
struct Bench {
    dir: TempDir,
    run: RunDir,
    work: PathBuf,
    vendor: PathBuf,
}

impl Bench {
    async fn new(turns: &Value) -> (Self, Log) {
        let dir = TempDir::new("octet-fleet-step");
        let data = dir.path().join("data");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let script = dir.path().join("script.json");
        let body = json!({
            "log": dir.path().join("vendor-log"),
            "startup_marker": dir.path().join("marker"),
            "turns": turns,
        });
        std::fs::write(&script, body.to_string()).unwrap();
        let vendor = write_script(
            &dir.path().join("bin"),
            "vendor",
            &format!(
                "OCTET_FLEET_SCRIPT='{}' exec '{}' \"$@\"",
                script.display(),
                octet_testkit::protocol_child().display()
            ),
        );
        let run = store::create(&data, RUN, &run_file()).await.unwrap();
        let (log, records) = Log::open(&run).await.unwrap();
        assert!(records.is_empty());
        let bench = Self {
            dir,
            run,
            work,
            vendor,
        };
        (bench, log)
    }

    fn marker(&self) -> PathBuf {
        self.dir.path().join("marker")
    }

    fn attempt_dir(&self) -> PathBuf {
        store::attempt_dir(&self.run, AttemptId(0))
    }

    /// A step of `kind` on `engine`, running this bench's vendor.
    fn step(&self, engine: &str, kind: Kind) -> Step {
        let refs = match kind {
            Kind::Document | Kind::Verdict => Refs::Document { inputs: Vec::new() },
            Kind::Change | Kind::Tests => Refs::Change { inputs: Vec::new() },
        };
        Step {
            id: "work".into(),
            kind,
            instructions: "Do the work.".into(),
            profile: ResolvedProfile {
                engine: engine.into(),
                binary: self.vendor.clone(),
                model: None,
                effort: None,
                mode: "ask".into(),
                approval_timeout_secs: 60,
            },
            session: SessionPolicy::Fresh,
            refs,
        }
    }

    fn plan<'a>(&self, step: &'a Step, dispatch: &'a Dispatch, prompt: &str) -> AttemptPlan<'a> {
        AttemptPlan {
            step,
            dispatch,
            attempt: AttemptId(0),
            prompt: prompt.into(),
            cwd: self.work.clone(),
            attempt_dir: self.attempt_dir(),
            marker: format!("{RUN}/0"),
            data_dir: self.dir.path().join("data"),
        }
    }

    /// The log's records, read from the file.
    fn records(&self) -> Vec<Record> {
        read_records(&self.run.root.join("log.jsonl"))
    }
}

fn read_records(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn run_file() -> RunFile {
    RunFile {
        workflow: Resolved {
            name: "test".into(),
            limits: Limits {
                max_rounds: 5,
                transient_retries: 2,
            },
            steps: Vec::new(),
        },
        hash: "0123456789abcdef".into(),
        task: "a task".into(),
        top: PathBuf::from("/work/repo"),
        cwd_rel: PathBuf::new(),
        base: CandidateId(0),
        base_tree: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
        octet_version: "0.0.0-test".into(),
    }
}

fn dispatch() -> Dispatch {
    Dispatch {
        step: 0,
        round: 1,
        input_candidate: CandidateId(0),
        requires: Vec::new(),
        history: Vec::new(),
        resume_session: None,
        not_before: None,
    }
}

/// The prompt header the fake's script keys on, for step `work` round 1.
fn fleet_prompt() -> String {
    "[octet-fleet step=work round=1 attempt=0 candidate=0 tree=t0]\nDo the work.".into()
}

/// An approver that answers `allow` and counts how often it was asked.
struct Counting {
    allow: bool,
    asked: Arc<AtomicUsize>,
}

impl Approver for Counting {
    fn ask<'a>(
        &'a mut self,
        _summary: &'a ApprovalSummary,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let allow = self.allow;
        Box::pin(async move { allow })
    }
}

fn approver(allow: bool) -> (Counting, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counting = Counting {
        allow,
        asked: Arc::clone(&asked),
    };
    (counting, asked)
}

/// Runs the attempt, collecting every fleet event it sends.
async fn attempt(
    plan: AttemptPlan<'_>,
    log: &mut Log,
    approver: &mut dyn Approver,
) -> (Result<TurnEnd, StepError>, Vec<FleetEvent>) {
    let (tx, mut rx) = mpsc::channel(16);
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    // The runner may run an attempt on any task: its future must be `Send`.
    let running = is_send(run_attempt(plan, log, approver, &tx));
    let end = tokio::time::timeout(Duration::from_secs(60), running)
        .await
        .expect("the attempt ended");
    drop(tx);
    (end, collect.await.unwrap())
}

fn is_send<T: Send>(value: T) -> T {
    value
}

fn replied(end: Result<TurnEnd, StepError>) -> String {
    match end.unwrap() {
        TurnEnd::Replied { text } => text,
        other => panic!("not a reply: {other:?}"),
    }
}

async fn a_reply_through(engine: &str) {
    let (bench, mut log) = Bench::new(&json!({"work#1": {"reply": "scripted reply"}})).await;
    let step = bench.step(engine, Kind::Document);
    let dispatch = dispatch();
    let (mut approve, asked) = approver(true);
    let plan = bench.plan(&step, &dispatch, &fleet_prompt());
    let (end, events) = attempt(plan, &mut log, &mut approve).await;
    assert_eq!(replied(end), "scripted reply");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    // Every session event is forwarded under the attempt.
    assert!(events.iter().any(|event| matches!(
        event,
        FleetEvent::Session {
            attempt: AttemptId(0),
            event: Event::Finished { .. }
        }
    )));
    // The session is registered, under the gate, with the gate's PID.
    let records = bench.records();
    let Some(Record::AttemptSession {
        attempt,
        journal,
        identities,
    }) = records.first()
    else {
        panic!("{records:?}");
    };
    assert_eq!(*attempt, AttemptId(0));
    assert!(
        journal.starts_with(bench.attempt_dir()),
        "{}",
        journal.display()
    );
    let pids = octet_fleet::gate::read_processes(&bench.attempt_dir())
        .await
        .unwrap();
    assert!(!pids.is_empty());
    assert_eq!(
        identities.iter().map(|i| i.pid).collect::<Vec<_>>(),
        pids[..identities.len()]
    );
    assert!(identities.iter().all(|i| !i.start.is_empty()));
}

#[tokio::test]
async fn a_reply_is_collected() {
    a_reply_through("codex").await;
    a_reply_through("claude").await;
}

#[tokio::test]
async fn the_vendor_does_not_run_before_registration() {
    let (bench, mut log) = Bench::new(&json!({"work#1": {"reply": "ok"}})).await;
    let marker = bench.marker();
    let log_file = bench.run.root.join("log.jsonl");
    let registered_first = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&registered_first);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    // Watches for the vendor's first act, and on it reads the log.
    let watcher = std::thread::spawn(move || {
        while !stopping.load(Ordering::SeqCst) {
            if marker.exists() {
                let registered = read_records(&log_file)
                    .iter()
                    .any(|record| matches!(record, Record::AttemptSession { .. }));
                seen.store(registered, Ordering::SeqCst);
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    });
    let step = bench.step("codex", Kind::Document);
    let dispatch = dispatch();
    let (mut approve, _) = approver(true);
    let plan = bench.plan(&step, &dispatch, &fleet_prompt());
    let (end, _) = attempt(plan, &mut log, &mut approve).await;
    assert_eq!(replied(end), "ok");
    stop.store(true, Ordering::SeqCst);
    assert!(watcher.join().unwrap(), "the vendor never started");
    assert!(
        registered_first.load(Ordering::SeqCst),
        "the vendor ran before its session was in the log"
    );
}

#[tokio::test]
async fn read_only_approvals_are_denied() {
    let (bench, mut log) = Bench::new(&json!({})).await;
    let step = bench.step("codex", Kind::Verdict);
    let dispatch = dispatch();
    let (mut approve, asked) = approver(true);
    let plan = bench.plan(&step, &dispatch, scenario::APPROVAL);
    let (end, events) = attempt(plan, &mut log, &mut approve).await;
    assert_eq!(replied(end), "decline");
    assert_eq!(asked.load(Ordering::SeqCst), 0, "the approver was asked");
    assert!(events.iter().any(|event| matches!(
        event,
        FleetEvent::Session {
            event: Event::Approval { .. },
            ..
        }
    )));
}

#[tokio::test]
async fn write_approvals_go_to_the_approver() {
    for (allow, reply) in [(true, "accept"), (false, "decline")] {
        let (bench, mut log) = Bench::new(&json!({})).await;
        let step = bench.step("codex", Kind::Change);
        let dispatch = dispatch();
        let (mut approve, asked) = approver(allow);
        let plan = bench.plan(&step, &dispatch, scenario::APPROVAL);
        let (end, _) = attempt(plan, &mut log, &mut approve).await;
        assert_eq!(replied(end), reply);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn a_vendor_session_is_logged() {
    let (bench, mut log) = Bench::new(&json!({"work#1": {"reply": "ok"}})).await;
    let step = bench.step("codex", Kind::Document);
    let dispatch = dispatch();
    let (mut approve, _) = approver(true);
    let plan = bench.plan(&step, &dispatch, &fleet_prompt());
    let (end, events) = attempt(plan, &mut log, &mut approve).await;
    assert_eq!(replied(end), "ok");
    let ready: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            FleetEvent::Session {
                event: Event::Ready { session },
                ..
            } if !session.is_empty() => Some(session.as_str()),
            _ => None,
        })
        .collect();
    let last = *ready.last().expect("a vendor session ID");
    let logged: Vec<String> = bench
        .records()
        .into_iter()
        .filter_map(|record| match record {
            Record::VendorSession {
                attempt: AttemptId(0),
                id,
            } => Some(id),
            _ => None,
        })
        .collect();
    assert_eq!(logged.last().map(String::as_str), Some(last));
    // Logged once per change, never twice in a row.
    assert!(
        logged.windows(2).all(|pair| pair[0] != pair[1]),
        "{logged:?}"
    );
    // After the session, which is after the registration.
    let records = bench.records();
    assert!(
        matches!(records[0], Record::AttemptSession { .. }),
        "{records:?}"
    );
}

#[tokio::test]
async fn a_failed_turn_is_classified() {
    let (bench, mut log) = Bench::new(&json!({})).await;
    let step = bench.step("codex", Kind::Document);
    let dispatch = dispatch();
    let (mut approve, _) = approver(true);
    let plan = bench.plan(&step, &dispatch, scenario::FAIL);
    let (end, _) = attempt(plan, &mut log, &mut approve).await;
    match end.unwrap() {
        TurnEnd::Failed(FailureClass::Transient { reset_at: None }, detail) => {
            assert!(detail.contains("usage limit"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_relative_binary_is_a_config_error() {
    let (bench, mut log) = Bench::new(&json!({})).await;
    let mut step = bench.step("codex", Kind::Document);
    step.profile.binary = PathBuf::from("codex");
    let dispatch = dispatch();
    let (mut approve, _) = approver(true);
    let plan = bench.plan(&step, &dispatch, &fleet_prompt());
    let (end, _) = attempt(plan, &mut log, &mut approve).await;
    assert!(
        matches!(end.unwrap(), TurnEnd::Failed(FailureClass::ConfigError, _)),
        "a relative binary must not reach the gate"
    );
    assert!(bench.records().is_empty());
    assert!(!bench.attempt_dir().join("go").exists());
}

#[tokio::test]
async fn background_work_is_waited_for() {
    // Claude's background script: the turn ends with a task running, and when
    // it finishes Claude starts a turn of its own, whose reply is the answer.
    let (bench, mut log) = Bench::new(&json!({})).await;
    let step = bench.step("claude", Kind::Document);
    let dispatch = dispatch();
    let (mut approve, _) = approver(true);
    let plan = bench.plan(&step, &dispatch, scenario::BACKGROUND);
    let (end, _) = attempt(plan, &mut log, &mut approve).await;
    // The follow-up turn's reply replaces the first turn's ("waiting").
    assert_eq!(replied(end), scenario::BACKGROUND_REPLY);
}
