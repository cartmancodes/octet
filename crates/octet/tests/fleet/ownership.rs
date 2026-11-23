//! Release tests 7, 11 and 12, and a leftover test process found on
//! resume: who may write the work tree, and which processes are the run's.
//!
//! A leader that exits before its child is made exactly: the fake coder
//! starts its survivors and waits; the test stops Octet (SIGSTOP), lets the
//! fake reply and exit (Octet, stopped, never reads the reply), sees the
//! leader exit, and only then kills Octet.

use std::path::Path;

use octet_fleet::ownership::ProcessInfo;
use octet_fleet::record::{AttemptId, Decision, PauseReason, Record};
use serde_json::{Value, json};

use crate::common::{
    Bench, Octet, alive, await_exited, await_gone_of, await_path, await_until, blocking_once,
    exited, last_attempt_of, last_pause, lines_of, registered, small, small_turns, started,
    survivors, waiting,
};

/// The detail of the pause the log ends with.
fn pause_detail(records: &[Record]) -> &str {
    match records.last() {
        Some(Record::Paused { detail, .. }) => detail,
        other => panic!("the log ends with {other:?}, not a pause"),
    }
}

/// The decisions in the log, in order.
fn decisions(records: &[Record]) -> Vec<&Decision> {
    records
        .iter()
        .filter_map(|record| match record {
            Record::Decision { decision } => Some(decision),
            _ => None,
        })
        .collect()
}

/// A coder turn that writes `file`, starts a marked survivor (and, with
/// `unmarked`, one without the marker), waits for `release`, then replies
/// and exits.
fn leaving_coder(file: &str, unmarked: bool, release: &Path) -> Value {
    json!({
        "write": {file: "written by the coder\n"},
        "spawn_survivor": true,
        "unmarked_survivor": unmarked,
        "wait_for": release,
        "reply": "Done.",
        "exit_after": true,
    })
}

/// Runs the coder of `octet`'s run `run` to the point a `leaving_coder` turn
/// waits at; then the coder's leader exits before its children, while Octet
/// is stopped, and Octet is killed. The survivors, which outlive both.
fn leader_exits_then_octet_dies(
    bench: &Bench,
    octet: &mut Octet,
    run: &str,
    release: &Path,
) -> Vec<ProcessInfo> {
    await_path(octet, &waiting(release));
    let implement = last_attempt_of(&bench.records_of(run), "implement").unwrap();
    let leaders = registered(bench, run, implement);
    assert_eq!(leaders.len(), 1, "{leaders:?}");
    let leader = leaders[0];
    let left = survivors(bench);
    assert!(!left.is_empty());
    for survivor in &left {
        assert_eq!(survivor.pgid, leader, "{survivor:?}");
        assert_eq!(survivor.ppid, leader, "{survivor:?}");
    }
    octet.signal(libc::SIGSTOP);
    std::fs::write(release, "").unwrap();
    await_exited(leader, "the coder's leader");
    octet.sigkill();
    let records = bench.records_of(run);
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::AttemptFinished { attempt, .. } if *attempt == implement
        )),
        "{records:#?}"
    );
    for survivor in &left {
        assert!(alive(survivor.pid), "{survivor:?}");
    }
    left
}

// ---- 7. workspace ownership --------------------------------------------

/// The spec's release test 7, "Workspace ownership".
#[test]
fn a_second_run_is_refused_and_an_edit_to_a_paused_run_is_drift() {
    let bench = Bench::new("octet-fleet-workspace");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["research#1"] = json!({"hold": "before_write"});
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    await_until(&mut octet, "the research turn", || {
        !bench.plays_of("research#1").is_empty()
    });
    let run = bench.run_id();

    // A second run on the same work tree is refused, and leaves no run.
    let other = bench.run(&workflow, &[]).finish();
    other.expect_code(1);
    assert!(other.stderr.contains(&run), "{}", other.stderr);
    assert_eq!(bench.run_ids(), std::slice::from_ref(&run));

    // The run pauses (Ctrl+C); the tree is edited while it is paused.
    octet.signal(libc::SIGINT);
    octet.finish().expect_code(130);
    let paused = bench.records();
    assert_eq!(last_pause(&paused), PauseReason::Interrupted);
    std::fs::write(bench.work.join("stray.txt"), "an edit while paused\n").unwrap();

    // Drift, before the next step: nothing is started.
    bench.resume(None, &[]).finish().expect_code(2);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::Drift);
    assert!(
        pause_detail(&records).contains(&bench.live_tree()),
        "{records:#?}"
    );
    assert_eq!(started(&records), started(&paused));
    assert_eq!(bench.plays_of("research#1").len(), 1);
}

// ---- 11. a leader that exits before its child --------------------------

/// The spec's release test 11, "A leader that exits before its child".
#[test]
fn a_child_that_outlives_its_leader_is_found_and_only_it_is_killed() {
    let bench = Bench::new("octet-fleet-orphan");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let release = bench.side("release-coder");
    let mut turns = small_turns();
    turns["implement#1"] = leaving_coder("feature.txt", true, &release);
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    await_path(&mut octet, &waiting(&release));
    let run = bench.run_id();
    let implement = last_attempt_of(&bench.records(), "implement").unwrap();
    let left = leader_exits_then_octet_dies(&bench, &mut octet, &run, &release);
    assert_eq!(left.len(), 2, "{left:?}");
    let marker = format!("{run}/{}", implement.0);
    let marked = left
        .iter()
        .find(|survivor| survivor.marker.as_deref() == Some(marker.as_str()))
        .unwrap_or_else(|| panic!("no survivor carries {marker}: {left:?}"))
        .pid;
    let unmarked = left
        .iter()
        .find(|survivor| survivor.marker.is_none())
        .unwrap_or_else(|| panic!("no survivor without the marker: {left:?}"))
        .pid;

    // On resume the child is found by its marker: ProcessesRunning, listing
    // it alone, with its command.
    bench.resume(None, &[]).finish().expect_code(2);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::ProcessesRunning);
    let detail = pause_detail(&records);
    assert!(
        detail.contains(&format!("[{marked}]\n  {marked}: ")),
        "{records:#?}"
    );
    assert!(!detail.contains(&format!("{unmarked}:")), "{detail}");

    // `kill` signals it and nothing else; the check runs again and finds the
    // child without the marker: unverified, never signalled.
    bench.resume(Some("kill"), &[]).finish().expect_code(3);
    let records = bench.records();
    assert_eq!(decisions(&records), [&Decision::Kill]);
    assert!(exited(marked), "the marked child outlived the kill");
    assert!(alive(unmarked), "the unmarked child was signalled");
    assert_eq!(last_pause(&records), PauseReason::ProcessesUnverified);
    // Listed with its command, and alone.
    let detail = pause_detail(&records);
    assert!(detail.contains(&format!("[{unmarked}]")), "{detail}");
    assert!(
        detail.contains(&format!(
            "{unmarked}: {} fleet-survivor",
            octet_testkit::protocol_child().display()
        )),
        "{detail}"
    );
    assert!(!detail.contains(&format!("{marked}:")), "{detail}");

    // `stop` is refused until it is released.
    let refused = bench.stop_run(&run).finish();
    refused.expect_code(3);
    assert!(refused.stderr.contains("release"), "{}", refused.stderr);
    assert!(
        !bench
            .records()
            .iter()
            .any(|record| matches!(record, Record::Stopped))
    );
    assert!(alive(unmarked));
    bench.resume(Some("release"), &[]).finish().expect_code(3);
    let records = bench.records();
    assert_eq!(
        decisions(&records).last(),
        Some(&&Decision::Release {
            pids: vec![unmarked]
        })
    );
    // Released, the process is ignored: the coder's unfinished edit is what
    // is left to decide.
    assert_eq!(last_pause(&records), PauseReason::Reconcile);
    bench.stop_run(&run).finish().expect_code(0);
    assert!(matches!(bench.records().last(), Some(Record::Stopped)));
    assert!(alive(unmarked), "the released child was signalled");
    assert!(bench.listed_runs().is_empty(), "{:?}", bench.listed_runs());
}

// ---- 12. an older run behind a newer one --------------------------------

/// Run X, on the small workflow whose tests fail, pauses between attempts:
/// sent back, its coder changes nothing, and the run stalls. Nothing of it is
/// unresolved, so the work tree's record does not list it. Its ID.
fn older_run_pauses(bench: &Bench) -> String {
    let workflow = bench.workflow_named("x.yaml", &small(&["test -f absent.txt"], 5));
    let mut turns = small_turns();
    turns["implement#2"] = json!({"reply": "Nothing to change."});
    bench.script(&turns);
    bench.run(&workflow, &[]).finish().expect_code(3);
    let x = bench.run_id();
    let records = bench.records_of(&x);
    assert_eq!(last_pause(&records), PauseReason::Stalled);
    assert!(
        started(&records)
            .iter()
            .any(|(_, step)| *step == "implement")
    );
    assert!(bench.listed_runs().is_empty(), "{:?}", bench.listed_runs());
    x
}

/// Starts run Y on the bench's work tree, with `coder` as its coder's turn.
/// Its workflow and its ID.
fn newer_run(bench: &Bench, x: &str, coder: Value) -> (std::path::PathBuf, Octet, String) {
    let workflow = bench.workflow_named("y.yaml", &small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["implement#1"] = coder;
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    await_until(&mut octet, "run Y", || bench.run_ids().len() == 2);
    let y = bench.run_ids().into_iter().find(|run| run != x).unwrap();
    (workflow, octet, y)
}

/// `octet fleet resume X --decision continue`: refused, naming Y, with X's
/// log as it was.
fn older_is_refused(bench: &Bench, x: &str, y: &str) {
    let before = bench.records_of(x);
    let refused = bench.resume_run(x, Some("continue"), &[]).finish();
    refused.expect_code(1);
    assert!(refused.stderr.contains(y), "{}", refused.stderr);
    assert_eq!(bench.records_of(x), before);
}

/// Once Y is stopped, `resume X` reports Y's leftover changes as `Drift`:
/// X started nothing since it paused.
fn older_sees_drift(bench: &Bench, x: &str, file: &str) {
    let paused = bench.records_of(x);
    bench
        .resume_run(x, Some("continue"), &[])
        .finish()
        .expect_code(3);
    let records = bench.records_of(x);
    assert_eq!(last_pause(&records), PauseReason::Drift);
    assert_eq!(started(&records), started(&paused));
    assert!(bench.work.join(file).exists());
    assert!(
        pause_detail(&records).contains(&bench.live_tree()),
        "{records:#?}"
    );
}

/// The spec's release test 12, "An older run behind a newer one": Y killed
/// mid-implement, its coder holding after its edit.
#[test]
fn an_older_run_waits_behind_a_newer_unresolved_one() {
    let bench = Bench::new("octet-fleet-older");
    let x = older_run_pauses(&bench);
    let coder = json!({"write": {"y.txt": "y\n"}, "hold": "after_write"});
    let (_, mut octet, y) = newer_run(&bench, &x, coder);
    await_path(&mut octet, &bench.work.join("y.txt"));
    octet.sigkill();
    let implement = last_attempt_of(&bench.records_of(&y), "implement").unwrap();
    await_gone_of(&bench, &y, implement);
    assert_eq!(bench.listed_runs(), std::slice::from_ref(&y));

    // `resume X` is refused, naming Y, until Y is resumed or stopped:
    // resumed to its own pause, Y still has an unresolved writer.
    older_is_refused(&bench, &x, &y);
    bench.resume_run(&y, None, &[]).finish().expect_code(2);
    assert_eq!(last_pause(&bench.records_of(&y)), PauseReason::Reconcile);
    older_is_refused(&bench, &x, &y);

    // Once Y is stopped, `resume X` reports Y's leftover changes as Drift.
    bench.stop_run(&y).finish().expect_code(0);
    assert!(bench.listed_runs().is_empty(), "{:?}", bench.listed_runs());
    older_sees_drift(&bench, &x, "y.txt");
    assert_eq!(bench.run_ids().len(), 2);
}

/// The spec's release test 12, repeated with Y's coder leader exiting before
/// its child, and Octet killed while the child runs.
#[test]
fn an_older_run_waits_while_a_newer_runs_child_lives() {
    let bench = Bench::new("octet-fleet-older-child");
    let x = older_run_pauses(&bench);
    let release = bench.side("release-y");
    let coder = leaving_coder("y.txt", false, &release);
    let (workflow, mut octet, y) = newer_run(&bench, &x, coder);
    let left = leader_exits_then_octet_dies(&bench, &mut octet, &y, &release);
    assert_eq!(left.len(), 1, "{left:?}");
    let child = left[0].pid;
    assert!(
        left[0]
            .marker
            .as_deref()
            .is_some_and(|m| m.starts_with(&format!("{y}/")))
    );

    // `resume X` and a new `run` are both refused while the child lives.
    older_is_refused(&bench, &x, &y);
    let other = bench.run(&workflow, &[]).finish();
    other.expect_code(1);
    assert!(other.stderr.contains(&y), "{}", other.stderr);
    assert_eq!(bench.run_ids().len(), 2);
    // `stop Y` is refused while it is alive.
    let refused = bench.stop_run(&y).finish();
    refused.expect_code(3);
    assert!(
        refused.stderr.contains("still running"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains(&child.to_string()),
        "{}",
        refused.stderr
    );
    assert!(
        !bench
            .records_of(&y)
            .iter()
            .any(|record| matches!(record, Record::Stopped))
    );
    assert!(alive(child));

    // `resume Y` kills it (ProcessesRunning → kill); then Y is stopped.
    bench.resume_run(&y, None, &[]).finish().expect_code(2);
    assert_eq!(
        last_pause(&bench.records_of(&y)),
        PauseReason::ProcessesRunning
    );
    bench
        .resume_run(&y, Some("kill"), &[])
        .finish()
        .expect_code(3);
    assert!(exited(child), "the child outlived the kill");
    assert_eq!(last_pause(&bench.records_of(&y)), PauseReason::Reconcile);
    bench.stop_run(&y).finish().expect_code(0);

    // `resume X` proceeds to Drift.
    older_sees_drift(&bench, &x, "y.txt");
    assert_eq!(bench.run_ids().len(), 2);
}

// ---- a leftover test process found on resume ----------------------------

/// From the spec's "Plus" list: Octet is killed while a test command runs;
/// resume finds the command by its registered identity (`ProcessesRunning`),
/// `kill` ends it, and the evidence runs again from command zero.
#[test]
fn a_leftover_test_process_is_found_on_resume() {
    let bench = Bench::new("octet-fleet-leftover-test");
    let runs = bench.side("runs");
    let command = blocking_once(&bench, &runs);
    let workflow = bench.workflow(&small(&[command.as_str()], 1));
    bench.script(&small_turns());
    let mut octet = bench.run(&workflow, &[]);
    await_until(&mut octet, "the command", || lines_of(&runs) == 1);
    octet.sigkill();
    let records = bench.records();
    let tester = last_attempt_of(&records, "test").unwrap();
    let identity = records
        .iter()
        .find_map(|record| match record {
            Record::EvidenceCommand {
                attempt, identity, ..
            } if *attempt == tester => Some(identity.clone()),
            _ => None,
        })
        .unwrap();
    assert!(alive(identity.pid));

    // Found on resume, by the identity its `evidence_command` registered.
    bench.resume(None, &[]).finish().expect_code(2);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::ProcessesRunning);
    assert!(
        pause_detail(&records).contains(&identity.pid.to_string()),
        "{records:#?}"
    );
    // `kill` ends it; the evidence starts again from command zero.
    bench.resume(Some("kill"), &[]).finish().expect_code(0);
    assert!(exited(identity.pid));
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    let commands: Vec<(u32, usize)> = records
        .iter()
        .filter_map(|record| match record {
            Record::EvidenceCommand {
                attempt,
                restart,
                index,
                ..
            } if *attempt == tester => Some((*restart, *index)),
            _ => None,
        })
        .collect();
    assert_eq!(commands, [(0, 0), (0, 0)]);
    assert_eq!(lines_of(&runs), 2);
    assert_eq!(bench.plays_of("test#1").len(), 1);
    assert_eq!(
        started(&records)
            .iter()
            .filter(|(_, step)| *step == "test")
            .map(|(attempt, _)| *attempt)
            .collect::<Vec<AttemptId>>(),
        [tester]
    );
    bench.assert_passing_evidence_is_whole(&records, 1);
}
