//! Release tests 4, 5, 6 and 9: Octet killed at the acceptance boundary,
//! before a vendor's registration, with a tester interrupted, and at the
//! tester-to-evidence handoff. Each kill point is exact: a `git` tripwire
//! fires on the first git Octet runs after a turn (it runs none during one),
//! and the test pauses hold Octet at the registration and evidence points.

use std::path::Path;

use octet_fleet::record::{Accepted, AttemptId, CandidateId, Origin, PauseReason, Record};
use serde_json::json;

use crate::common::{
    Bench, SHORT, alive, await_exited, await_gone, await_path, git_tripwire, last_attempt_of,
    last_pause, last_phase, lines_of, logged, observations, origin_of, pause_at, pause_env,
    played_turn, registered, sleep_hold, small, small_turns, started,
};

/// The attempts of `step` started in `records`, in order.
fn attempts_of(records: &[Record], step: &str) -> Vec<AttemptId> {
    started(records)
        .into_iter()
        .filter(|(_, s)| *s == step)
        .map(|(attempt, _)| attempt)
        .collect()
}

/// Whether `attempt` was ever accepted.
fn finished(records: &[Record], attempt: AttemptId) -> bool {
    records
        .iter()
        .any(|record| matches!(record, Record::AttemptFinished { attempt: a, .. } if *a == attempt))
}

/// Whether the log ends with a `completed` record.
fn completed(records: &[Record]) -> bool {
    matches!(records.last(), Some(Record::Completed { .. }))
}

// ---- 4. crash at the acceptance boundary -------------------------------

/// The spec's release test 4, first half: Octet is killed after the coder's
/// turn has ended (its vendor shut down) and before `attempt_finished` is
/// durable, at the first git Octet runs to take the change.
#[test]
fn a_crash_before_acceptance_reconciles() {
    let bench = Bench::new("octet-fleet-accept-before");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    bench.script(&small_turns());
    let (path, fired) = git_tripwire(&bench, &played_turn(&bench, "implement#1"));
    let ended = bench.run(&workflow, &[("PATH", path.as_os_str())]).finish();
    assert!(fired.exists(), "the tripwire never fired");
    assert_eq!(ended.code, None, "octet was not killed: {}", ended.stderr);

    // The child finished: its turn played, its processes are gone. The
    // attempt was not accepted, and its change was never proposed.
    let records = bench.records();
    let implement = last_attempt_of(&records, "implement").unwrap();
    assert_eq!(bench.plays_of("implement#1").len(), 1);
    await_gone(&bench, implement);
    assert!(!finished(&records, implement), "{records:#?}");
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::Candidate {
                origin: Origin::Change,
                ..
            }
        )),
        "{records:#?}"
    );
    assert!(bench.work.join("feature.txt").exists());

    // Reconciliation: the coder's tree is in place and its attempt
    // unfinished, so the run pauses for a decision, running nothing.
    bench.resume(None, &[]).finish().expect_code(2);
    let after = bench.records();
    assert_eq!(last_pause(&after), PauseReason::Reconcile);
    assert_eq!(started(&after), started(&records));

    // Adopted, the tree is the change: the coder never runs again.
    bench.resume(Some("adopt"), &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(completed(&records), "{records:#?}");
    assert_eq!(attempts_of(&records, "implement"), [implement]);
    assert_eq!(bench.plays_of("implement#1").len(), 1);
    let change = records
        .iter()
        .find_map(|record| match record {
            Record::AttemptFinished {
                attempt,
                outcome: Accepted::ChangeProduced { .. },
                candidate: Some(candidate),
            } if *attempt == implement => Some(*candidate),
            _ => None,
        })
        .expect("the adopted change was accepted");
    assert_eq!(origin_of(&records, change), Origin::Change);
}

/// The spec's release test 4, second half: Octet is killed once the coder's
/// `attempt_finished` is durable, at the first git it runs after it.
#[test]
fn a_crash_after_acceptance_skips_the_step() {
    let bench = Bench::new("octet-fleet-accept-after");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    bench.script(&small_turns());
    let (path, fired) = git_tripwire(&bench, &logged(&bench, "\"type\":\"change_produced\""));
    let ended = bench.run(&workflow, &[("PATH", path.as_os_str())]).finish();
    assert!(fired.exists(), "the tripwire never fired");
    assert_eq!(ended.code, None, "octet was not killed: {}", ended.stderr);
    let records = bench.records();
    let implement = last_attempt_of(&records, "implement").unwrap();
    assert!(finished(&records, implement), "{records:#?}");
    assert_eq!(attempts_of(&records, "test"), []);

    // Resume skips the step: the tests come next, the coder never again.
    bench.resume(None, &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(completed(&records), "{records:#?}");
    assert_eq!(attempts_of(&records, "implement"), [implement]);
    assert_eq!(bench.plays_of("implement#1").len(), 1);
    let next = started(&records)
        .into_iter()
        .skip_while(|(attempt, _)| *attempt != implement)
        .nth(1)
        .unwrap();
    assert_eq!(next.1, "test");
}

// ---- 5. crash before registration --------------------------------------

/// The spec's release test 5 at one kill point, `point`: `session` holds
/// Octet with the coder's gate spawned and registered, before
/// `attempt_session` is durable; `go`, after it and before `go`.
///
/// The gate is held in its poll's `sleep` before Octet is killed, so it is
/// still alive while both entry points are tried; let go on, it sees Octet
/// gone and exits by itself.
fn crash_before_registration(name: &str, point: &str) {
    let bench = Bench::new(name);
    let pauses = bench.side("pauses");
    std::fs::create_dir_all(&pauses).unwrap();
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    bench.script(&small_turns());
    let (path, hold, held) = sleep_hold(&bench);
    let [pause] = pause_env(&pauses);
    let mut octet = bench.run(&workflow, &[pause, ("PATH", path.as_os_str())]);
    let prefix = format!("{point}-");
    pause_at(&mut octet, &pauses, |held| {
        held.strip_prefix(&prefix)
            .and_then(|attempt| attempt.parse().ok())
            .is_some_and(|attempt| {
                last_attempt_of(&bench.records(), "implement") == Some(AttemptId(attempt))
            })
    });
    let run = bench.run_id();
    let records = bench.records();
    let implement = last_attempt_of(&records, "implement").unwrap();
    let session = records.iter().any(
        |record| matches!(record, Record::AttemptSession { attempt, .. } if *attempt == implement),
    );
    assert_eq!(session, point == "go", "{records:#?}");
    let go = bench.attempt_dir(implement).join("go");
    assert!(!go.exists());
    let gates = registered(&bench, &run, implement);
    assert_eq!(gates.len(), 1, "{gates:?}");
    let gate = gates[0];
    // Held in its poll's `sleep`, the gate cannot see Octet die.
    std::fs::write(&hold, "").unwrap();
    assert!(
        octet_testkit::wait_until(SHORT, || lines_of(&held) > 0),
        "the gate never polled"
    );
    assert_eq!(std::fs::read_to_string(&held).unwrap(), format!("{gate}\n"));
    octet.sigkill();

    // `resume` admits no writer while the gate is alive: it pauses on the
    // run's leftover process and starts nothing.
    bench.resume(None, &[]).finish().expect_code(2);
    let after = bench.records();
    assert_eq!(last_pause(&after), held_gate_pause(point), "{after:#?}");
    assert_eq!(started(&after), started(&records));
    // Nor does a new run: the work tree is still this run's.
    let other = bench.run(&workflow, &[]).finish();
    other.expect_code(2);
    assert!(other.stderr.contains(&run), "{}", other.stderr);
    assert_eq!(bench.run_ids(), std::slice::from_ref(&run));
    assert!(alive(gate));
    // The vendor never executed: no startup marker for the gate's process,
    // no turn, no `go`.
    assert!(!bench.started_pids().contains(&gate));
    assert!(bench.plays_of("implement#1").is_empty());
    assert!(!go.exists());

    // Let go on, the waiting gate exits on its own, still without running
    // the vendor.
    std::fs::remove_file(&hold).unwrap();
    await_exited(gate, "the waiting gate");
    assert!(!bench.started_pids().contains(&gate));
    assert!(bench.plays_of("implement#1").is_empty());

    // The vendor never ran and the tree is unchanged: a new attempt.
    bench.resume(None, &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(completed(&records), "{records:#?}");
    let coders = attempts_of(&records, "implement");
    assert_eq!(coders.len(), 2, "{records:#?}");
    assert_eq!(coders[0], implement);
    assert!(!finished(&records, implement));
    assert_eq!(bench.plays_of("implement#1").len(), 1);
    let ran = registered(&bench, &run, coders[1]);
    assert!(
        ran.iter().any(|pid| bench.started_pids().contains(pid)),
        "{ran:?}"
    );
}

/// The pause `resume` makes for a gate held at `point`. Past `session` the
/// gate's identity is registered: verified. Before it, the gate is verified
/// only through what it shows: on Linux its held `sleep` carries the marker
/// it exported; on macOS `ps` shows no environment for `sh`, so nothing in
/// its registered group is verified.
fn held_gate_pause(point: &str) -> PauseReason {
    if point == "go" || cfg!(target_os = "linux") {
        PauseReason::ProcessesRunning
    } else {
        PauseReason::ProcessesUnverified
    }
}

/// The spec's release test 5, killed before `attempt_session` is durable.
#[test]
fn a_crash_before_the_session_record_never_runs_the_vendor() {
    crash_before_registration("octet-fleet-unregistered", "session");
}

/// The spec's release test 5, killed after `attempt_session` and before `go`.
#[test]
fn a_crash_before_go_never_runs_the_vendor() {
    crash_before_registration("octet-fleet-unreleased", "go");
}

// ---- 6. interrupted tester ---------------------------------------------

/// The spec's release test 6 up to the pause: the tester writes its test and
/// holds; Octet is killed before any command; resume finds the tester's
/// edit and pauses `Reconcile`. The command appends to `ran` (outside the
/// work tree) whenever it runs.
fn interrupted_tester(name: &str) -> (Bench, std::path::PathBuf) {
    let bench = Bench::new(name);
    let ran = bench.side("ran");
    let command = format!("echo ran >> '{}' && test -f feature_test.sh", ran.display());
    let workflow = bench.workflow(&small(&[command.as_str()], 1));
    let mut turns = small_turns();
    let test = json!({"feature_test.sh": "test -f feature.txt\n"});
    turns["test#1"] = json!([
        {"write": test, "hold": "after_write"},
        {"write": test, "reply": "Added feature_test.sh."},
    ]);
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    await_path(&mut octet, &bench.work.join("feature_test.sh"));
    octet.sigkill();
    let records = bench.records();
    let tester = last_attempt_of(&records, "test").unwrap();
    await_gone(&bench, tester);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::EvidenceStarted { .. })),
        "{records:#?}"
    );
    bench.resume(None, &[]).finish().expect_code(2);
    assert_eq!(last_pause(&bench.records()), PauseReason::Reconcile);
    assert!(!ran.exists());
    (bench, ran)
}

/// Asserts that every passing evidence record of the run is one whose
/// commands ran: each of its commands has its `evidence_command` and
/// `evidence_result`, and `ran` has a line for every command run.
fn passing_evidence_ran(bench: &Bench, ran: &Path) {
    let records = bench.records();
    let results = records
        .iter()
        .filter(|record| matches!(record, Record::EvidenceResult { .. }))
        .count();
    assert_eq!(lines_of(ran), results, "{records:#?}");
    let mut passes = 0;
    for record in &records {
        if let Record::AttemptFinished {
            attempt,
            outcome: Accepted::TestsPassed { .. },
            ..
        } = record
        {
            passes += 1;
            let (_, restart) = last_phase(&records, *attempt);
            let (pre, post) = observations(&records, *attempt, restart, 0);
            assert!(pre.is_some() && post.is_some(), "{records:#?}");
        }
    }
    assert!(passes > 0 && lines_of(ran) > 0, "{records:#?}");
    bench.assert_passing_evidence_is_whole(&records, 1);
}

/// The spec's release test 6: no decision yields passing evidence without
/// the commands running. `keep`: the tester goes on, then the commands run.
#[test]
fn an_interrupted_tester_kept_passes_only_by_running_the_commands() {
    let (bench, ran) = interrupted_tester("octet-fleet-tester-keep");
    bench.resume(Some("keep"), &[]).finish().expect_code(0);
    assert!(completed(&bench.records()));
    assert_eq!(bench.plays_of("test#1").len(), 2);
    passing_evidence_ran(&bench, &ran);
}

/// The spec's release test 6: `adopt` makes the tester's edit a candidate,
/// and the commands run on it.
#[test]
fn an_interrupted_tester_adopted_passes_only_by_running_the_commands() {
    let (bench, ran) = interrupted_tester("octet-fleet-tester-adopt");
    bench.resume(Some("adopt"), &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(completed(&records));
    assert_eq!(bench.plays_of("test#1").len(), 1);
    let tester = last_attempt_of(&records, "test").unwrap();
    let (candidate, _) = last_phase(&records, tester);
    assert_eq!(origin_of(&records, candidate), Origin::Tests);
    passing_evidence_ran(&bench, &ran);
}

/// The spec's release test 6: every other decision is refused, and `stop`
/// ends the run; none of them yields evidence.
#[test]
fn an_interrupted_tester_gets_no_evidence_from_any_other_decision() {
    let (bench, ran) = interrupted_tester("octet-fleet-tester-other");
    let before = bench.records();
    for decision in ["continue", "retry", "kill", "wait", "release"] {
        let ended = bench.resume(Some(decision), &[]).finish();
        ended.expect_code(2);
        assert!(ended.stderr.contains("keep, adopt"), "{}", ended.stderr);
        assert_eq!(bench.records(), before, "{decision}");
    }
    let ended = bench
        .resume_args(&["--decision", "rounds", "--rounds", "3"])
        .finish();
    ended.expect_code(2);
    assert_eq!(bench.records(), before);
    bench.stop_run(&bench.run_id()).finish().expect_code(0);
    let records = bench.records();
    assert!(
        matches!(records.last(), Some(Record::Stopped)),
        "{records:#?}"
    );
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::EvidenceStarted { .. }
                | Record::EvidenceCommand { .. }
                | Record::AttemptFinished {
                    outcome: Accepted::TestsPassed { .. },
                    ..
                }
        )),
        "{records:#?}"
    );
    assert!(!ran.exists());
}

// ---- 9. the tester-to-evidence handoff ---------------------------------

/// The tests run on the tester's turn: `edited` writes a test file.
fn handoff_bench(name: &str, edited: bool) -> (Bench, std::path::PathBuf) {
    let bench = Bench::new(name);
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    if edited {
        turns["test#1"] = json!({
            "write": {"feature_test.sh": "test -f feature.txt\n"},
            "reply": "Added feature_test.sh."
        });
    }
    bench.script(&turns);
    (bench, workflow)
}

/// The spec's release test 9, killed immediately before `evidence_started`
/// is durable: the tester's turn has ended, and the first git Octet runs to
/// take its tree kills it.
fn killed_before_the_handoff(name: &str, edited: bool) {
    let (bench, workflow) = handoff_bench(name, edited);
    let (path, fired) = git_tripwire(&bench, &played_turn(&bench, "test#1"));
    let ended = bench.run(&workflow, &[("PATH", path.as_os_str())]).finish();
    assert!(fired.exists(), "the tripwire never fired");
    assert_eq!(ended.code, None, "octet was not killed: {}", ended.stderr);
    let records = bench.records();
    let tester = last_attempt_of(&records, "test").unwrap();
    await_gone(&bench, tester);
    assert_eq!(bench.plays_of("test#1").len(), 1);
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::EvidenceStarted { .. }
                | Record::Candidate {
                    origin: Origin::Tests,
                    ..
                }
        )),
        "{records:#?}"
    );
    if edited {
        // The tester changed the tree, and nothing committed it: Reconcile.
        bench.resume(None, &[]).finish().expect_code(2);
        let after = bench.records();
        assert_eq!(last_pause(&after), PauseReason::Reconcile);
        assert_eq!(started(&after), started(&records));
        assert_eq!(bench.plays_of("test#1").len(), 1);
    } else {
        // Nothing changed: a new tester attempt.
        bench.resume(None, &[]).finish().expect_code(0);
        let records = bench.records();
        assert!(completed(&records), "{records:#?}");
        let testers = attempts_of(&records, "test");
        assert_eq!(testers.len(), 2, "{records:#?}");
        assert_eq!(testers[0], tester);
        assert!(!finished(&records, tester));
        assert_eq!(bench.plays_of("test#1").len(), 2);
        bench.assert_passing_evidence_is_whole(&records, 1);
    }
}

/// The spec's release test 9, killed immediately after `evidence_started` is
/// durable: the evidence runner holds command zero, spawned and unreleased.
fn killed_after_the_handoff(name: &str, edited: bool) {
    let (bench, workflow) = handoff_bench(name, edited);
    let pauses = bench.side("pauses");
    std::fs::create_dir_all(&pauses).unwrap();
    let mut octet = bench.run(&workflow, &pause_env(&pauses));
    pause_at(&mut octet, &pauses, |held| held == "0-0");
    octet.sigkill();
    let records = bench.records();
    let tester = last_attempt_of(&records, "test").unwrap();
    let implement = last_attempt_of(&records, "implement").unwrap();
    let committed = match records.last() {
        Some(Record::EvidenceStarted {
            attempt,
            candidate,
            restart: 0,
        }) if *attempt == tester => *candidate,
        other => panic!("the log ends with {other:?}: {records:#?}"),
    };
    let coded = records
        .iter()
        .find_map(|record| match record {
            Record::AttemptFinished {
                attempt,
                candidate: Some(candidate),
                ..
            } if *attempt == implement => Some(*candidate),
            _ => None,
        })
        .unwrap();
    if edited {
        assert_ne!(committed, coded);
        assert_eq!(origin_of(&records, committed), Origin::Tests);
    } else {
        assert_eq!(committed, coded);
    }
    await_gone(&bench, tester);

    // The evidence phase from command zero on the committed candidate; the
    // tester does not run again.
    bench.resume(None, &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(completed(&records), "{records:#?}");
    assert_eq!(attempts_of(&records, "test"), [tester]);
    assert_eq!(bench.plays_of("test#1").len(), 1);
    let phases: Vec<(CandidateId, u32)> = records
        .iter()
        .filter_map(|record| match record {
            Record::EvidenceStarted {
                attempt,
                candidate,
                restart,
            } if *attempt == tester => Some((*candidate, *restart)),
            _ => None,
        })
        .collect();
    assert_eq!(phases, [(committed, 0)]);
    let commands = records
        .iter()
        .filter(|record| matches!(record, Record::EvidenceCommand { attempt, .. } if *attempt == tester))
        .count();
    assert_eq!(commands, 1, "{records:#?}");
    let Some(Record::Completed { candidate, .. }) = records.last() else {
        unreachable!()
    };
    assert_eq!(*candidate, committed);
    bench.assert_passing_evidence_is_whole(&records, 1);
}

/// The spec's release test 9, killed immediately before `evidence_started`
/// is durable, with the tester's edit already proposed: Octet is held at the
/// handoff's test pause, the `candidate{origin: tests}` record its last. A
/// candidate record is only a proposal: recovery sees a tester whose edit
/// nothing committed.
#[test]
fn a_crash_with_the_testers_candidate_only_proposed_reconciles() {
    let (bench, workflow) = handoff_bench("octet-fleet-handoff-proposed", true);
    let pauses = bench.side("pauses");
    std::fs::create_dir_all(&pauses).unwrap();
    let mut octet = bench.run(&workflow, &pause_env(&pauses));
    pause_at(&mut octet, &pauses, |held| held.starts_with("handoff-"));
    octet.sigkill();
    let records = bench.records();
    let tester = last_attempt_of(&records, "test").unwrap();
    assert!(
        matches!(
            records.last(),
            Some(Record::Candidate {
                origin: Origin::Tests,
                ..
            })
        ),
        "{records:#?}"
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::EvidenceStarted { .. })),
        "{records:#?}"
    );
    await_gone(&bench, tester);

    // "Tests attempt, no evidence_started, tree changed": Reconcile, and no
    // attempt starts.
    bench.resume(None, &[]).finish().expect_code(2);
    let after = bench.records();
    assert_eq!(last_pause(&after), PauseReason::Reconcile);
    assert_eq!(started(&after), started(&records));
    assert_eq!(bench.plays_of("test#1").len(), 1);
    assert!(
        !after
            .iter()
            .any(|record| matches!(record, Record::EvidenceStarted { .. })),
        "{after:#?}"
    );
}

/// Release test 9: before the handoff, with a tester that edited the tree.
#[test]
fn a_crash_before_the_handoff_of_an_edit_reconciles() {
    killed_before_the_handoff("octet-fleet-handoff-before-edit", true);
}

/// Release test 9: before the handoff, with a tester that edited nothing.
#[test]
fn a_crash_before_the_handoff_of_no_edit_runs_a_new_tester() {
    killed_before_the_handoff("octet-fleet-handoff-before-none", false);
}

/// Release test 9: after the handoff, with a tester that edited the tree.
#[test]
fn a_crash_after_the_handoff_of_an_edit_runs_the_evidence() {
    killed_after_the_handoff("octet-fleet-handoff-after-edit", true);
}

/// Release test 9: after the handoff, with a tester that edited nothing.
#[test]
fn a_crash_after_the_handoff_of_no_edit_runs_the_evidence() {
    killed_after_the_handoff("octet-fleet-handoff-after-none", false);
}
