//! Release test 13, "Clean interrupts": Ctrl+C (SIGINT) during research,
//! after a coder edit, and during evidence; and to the whole process group,
//! as a terminal sends it, while a snapshot's git runs. Each persisted log
//! maps to exactly one row of the recovery table, and none asks for a
//! decision about the interrupt itself.

use octet_fleet::record::{AttemptId, PauseReason, Record};
use serde_json::json;

use crate::common::{
    Bench, Octet, await_path, await_until, blocking_once, git_hold, last_attempt_of, last_pause,
    lines_of, played_turn, small, small_turns, started,
};

/// Interrupts `octet` with SIGINT: it exits 130, and its log ends with the
/// running attempt interrupted (not finished) and the run paused for it.
/// That attempt.
fn interrupt(bench: &Bench, mut octet: Octet, step: &str) -> AttemptId {
    octet.signal(libc::SIGINT);
    octet.finish().expect_code(130);
    let records = bench.records();
    let attempt = last_attempt_of(&records, step).unwrap();
    let tail = &records[records.len() - 2..];
    assert!(
        matches!(
            tail,
            [
                Record::AttemptInterrupted { attempt: a, signal },
                Record::Paused {
                    reason: PauseReason::Interrupted,
                    ..
                },
            ] if *a == attempt && signal == "SIGINT"
        ),
        "{records:#?}"
    );
    assert!(
        !records.iter().any(|record| matches!(
            record,
            Record::AttemptFinished { attempt: a, .. } | Record::AttemptFailed { attempt: a, .. }
                if *a == attempt
        )),
        "{records:#?}"
    );
    attempt
}

/// Asserts the log holds one interrupt pause and no decision: `resume` went
/// past the interrupt without being asked about it.
fn no_decision_about_the_interrupt(records: &[Record]) {
    let interrupts = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                Record::Paused {
                    reason: PauseReason::Interrupted,
                    ..
                }
            )
        })
        .count();
    assert_eq!(interrupts, 1, "{records:#?}");
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::Decision { .. })),
        "{records:#?}"
    );
}

/// Ctrl+C during research: `resume` runs a new research attempt in a fresh
/// clone (the row "read-only attempt unfinished").
#[test]
fn an_interrupt_during_research_runs_a_new_attempt_in_a_fresh_clone() {
    let bench = Bench::new("octet-fleet-sigint-research");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["research#1"] = json!([
        {"hold": "before_write"},
        {"reply": "The repository holds a README and fixture.txt."},
    ]);
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    await_until(&mut octet, "the research turn", || {
        !bench.plays_of("research#1").is_empty()
    });
    let first = interrupt(&bench, octet, "research");
    let old_clone = bench.attempt_dir(first).join("tree");

    bench.resume(None, &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    no_decision_about_the_interrupt(&records);
    // The next attempt is a new one of the same step and round.
    let after: Vec<(AttemptId, &str)> = started(&records)
        .into_iter()
        .skip_while(|(attempt, _)| *attempt != first)
        .collect();
    assert_eq!(after[1].1, "research", "{records:#?}");
    let second = after[1].0;
    let rounds: Vec<u32> = records
        .iter()
        .filter_map(|record| match record {
            Record::AttemptStarted { attempt, round, .. }
                if *attempt == first || *attempt == second =>
            {
                Some(*round)
            }
            _ => None,
        })
        .collect();
    assert_eq!(rounds, [1, 1]);
    // In a fresh clone: its own, at the attempt's directory; the
    // interrupted attempt's clone is discarded.
    let plays = bench.plays_of("research#1");
    assert_eq!(plays.len(), 2);
    let clone_of = |attempt: AttemptId| format!("attempts/{}/tree", attempt.0);
    let cwds: Vec<&str> = plays
        .iter()
        .map(|play| play["cwd"].as_str().unwrap())
        .collect();
    assert!(cwds[0].ends_with(&clone_of(first)), "{plays:#?}");
    assert!(cwds[1].ends_with(&clone_of(second)), "{plays:#?}");
    assert!(
        !bench.attempt_dir(second).join("tree").exists(),
        "an accepted attempt's clone is removed"
    );
    assert!(
        !old_clone.exists(),
        "the interrupted attempt's clone is kept"
    );
}

/// Ctrl+C after a coder edit: `resume` pauses `Reconcile` (the row "change
/// attempt unfinished, released, tree changed").
#[test]
fn an_interrupt_after_a_coder_edit_reconciles() {
    let bench = Bench::new("octet-fleet-sigint-coder");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["implement#1"] = json!({
        "write": {"feature.txt": "the feature\n"},
        "hold": "after_write"
    });
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    await_path(&mut octet, &bench.work.join("feature.txt"));
    let coder = interrupt(&bench, octet, "implement");
    let interrupted = bench.records();

    // The interrupt needs no decision; the coder's edit does.
    bench.resume(None, &[]).finish().expect_code(2);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::Reconcile);
    no_decision_about_the_interrupt(&records);
    assert_eq!(started(&records), started(&interrupted));
    assert_eq!(last_attempt_of(&records, "implement"), Some(coder));
}

/// Ctrl+C during evidence: `resume` runs the evidence phase from command
/// zero, and the tester is not invoked (the row "tests attempt,
/// `evidence_started` durable").
#[test]
fn an_interrupt_during_evidence_runs_the_evidence_again_from_command_zero() {
    let bench = Bench::new("octet-fleet-sigint-evidence");
    let runs = bench.side("runs");
    let command = blocking_once(&bench, &runs);
    let workflow = bench.workflow(&small(&[command.as_str()], 1));
    bench.script(&small_turns());
    let mut octet = bench.run(&workflow, &[]);
    await_until(&mut octet, "the command", || lines_of(&runs) == 1);
    let tester = interrupt(&bench, octet, "test");

    bench.resume(None, &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    no_decision_about_the_interrupt(&records);
    assert_eq!(bench.plays_of("test#1").len(), 1);
    let testers: Vec<AttemptId> = started(&records)
        .into_iter()
        .filter(|(_, step)| *step == "test")
        .map(|(attempt, _)| attempt)
        .collect();
    assert_eq!(testers, [tester]);
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
    bench.assert_passing_evidence_is_whole(&records, 1);
}

/// Ctrl+C from a terminal reaches the whole foreground process group. Sent
/// while Octet waits for the `git` of the snapshot that accepts the coder's
/// finished turn, it reaches only Octet: that `git` (in a group of its own)
/// completes, the coder's change is accepted, and the run pauses for the
/// interrupt with nothing failed. `resume` needs no decision and completes.
#[test]
fn a_group_interrupt_during_a_snapshot_leaves_the_snapshot_whole() {
    let bench = Bench::new("octet-fleet-sigint-group");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    bench.script(&small_turns());
    // Octet runs no git while a vendor's turn runs: the first git after the
    // coder's turn is played is the acceptance snapshot's.
    let hold = git_hold(&bench, &played_turn(&bench, "implement#1"), false);
    let mut octet = bench.run(&workflow, &[("PATH", hold.path.as_os_str())]);
    await_path(&mut octet, &hold.held);
    octet.signal_group(libc::SIGINT);
    std::fs::write(&hold.go, "").unwrap();
    octet.finish().expect_code(130);
    assert!(hold.survived.exists(), "the held git died of the interrupt");

    let records = bench.records();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::AttemptFailed { .. })),
        "{records:#?}"
    );
    let implement = last_attempt_of(&records, "implement").unwrap();
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::AttemptFinished { attempt, .. } if *attempt == implement
        )),
        "{records:#?}"
    );
    assert_eq!(last_pause(&records), PauseReason::Interrupted);

    bench.resume(None, &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    no_decision_about_the_interrupt(&records);
    assert_eq!(bench.plays_of("implement#1").len(), 1);
}

/// Belt and braces: a snapshot that fails while a signal is pending (its
/// git, say, killed by the same Ctrl+C) interrupts the attempt rather than
/// failing it. The coder's edit is then an interrupted attempt's, which
/// `resume` reconciles, asking nothing about the interrupt.
#[test]
fn a_snapshot_that_fails_with_a_signal_pending_interrupts_the_attempt() {
    let bench = Bench::new("octet-fleet-sigint-failed-snapshot");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    bench.script(&small_turns());
    let hold = git_hold(&bench, &played_turn(&bench, "implement#1"), true);
    let mut octet = bench.run(&workflow, &[("PATH", hold.path.as_os_str())]);
    await_path(&mut octet, &hold.held);
    octet.signal(libc::SIGINT);
    std::fs::write(&hold.go, "").unwrap();
    let implement = interrupt(&bench, octet, "implement");
    assert!(hold.survived.exists());
    let records = bench.records();
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, Record::AttemptFailed { .. })),
        "{records:#?}"
    );

    bench.resume(None, &[]).finish().expect_code(2);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::Reconcile);
    no_decision_about_the_interrupt(&records);
    assert_eq!(last_attempt_of(&records, "implement"), Some(implement));
}
