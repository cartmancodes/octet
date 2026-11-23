//! Release tests 1, 2, 3, 8 and 10: the full run and the evidence
//! guarantee.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use octet_fleet::evidence::TEST_PAUSE_ENV;
use octet_fleet::record::{Accepted, AttemptId, Origin, PauseReason, Record};
use serde_json::json;

use crate::common::{
    Bench, answer_pauses, approve, await_gone, exit_of, failed, git, last_pause, last_phase,
    observations, only_test_attempt, origin_of, passed, position, research_plan_build, small,
    small_turns, started, tree_of, verdict,
};

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
    await_gone(&bench, killed);

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
    // B did run and pass: in pass 1, on the tree A changed.
    assert_eq!(exit_of(&records, attempt, 1, 1), Some(0), "{records:#?}");
    let (b_pre, b_post) = observations(&records, attempt, 1, 1);
    assert!(b_pre.is_some() && b_pre == b_post, "{records:#?}");
    assert_eq!(b_pre, post, "B ran on the tree A left: {records:#?}");
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
        evidence,
        observed_tree,
        ..
    }) = records.last()
    else {
        panic!("{records:#?}");
    };
    assert_eq!(observed_tree, tree);
    assert_eq!(*tree, bench.live_tree());
    assert_eq!(origin_of(&records, *candidate), Origin::Drift);
    let body = bench.evidence(*evidence);
    assert_eq!(body["candidate"], candidate.0);
    assert_eq!(body["tree"], tree.as_str());
    // After the drift, the tests and the final review ran again; the coder
    // did not.
    let drift = position(&records, |r| {
        matches!(
            r,
            Record::Paused {
                reason: PauseReason::Drift,
                ..
            }
        )
    })
    .unwrap();
    let again: Vec<&str> = started(&records[drift..])
        .into_iter()
        .map(|(_, step)| step)
        .collect();
    assert_eq!(again, ["test", "review_change"], "{records:#?}");
    assert_eq!(bench.plays_of("test#2").len(), 1);
    assert_eq!(bench.plays_of("review_change#2").len(), 1);
    let listed = git(&bench.work, &["ls-tree", "--name-only", tree]);
    assert!(listed.lines().any(|name| name == "late.txt"), "{listed}");
    bench.assert_passing_evidence_is_whole(&records, commands.len());
}

// ---- 10. an edit between commands --------------------------------------

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
