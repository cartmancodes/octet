//! From the spec's "Plus" list: a stall pause, a round-limit pause, an
//! access violation and `UnstableTests → retry`, each paused (exit 3) and
//! then decided on to completion.

use octet_fleet::record::{Decision, Failure, PauseReason, Record};
use serde_json::json;

use crate::common::{
    Bench, approve, last_attempt_of, last_pause, small, small_turns, started, verdict,
    verdict_previous,
};

/// How many times `step` was attempted in `records`.
fn attempts(records: &[Record], step: &str) -> usize {
    started(records).iter().filter(|(_, s)| *s == step).count()
}

/// A stall: the research review asks for changes again and resolves none of
/// its open findings. `continue` takes the pending jump.
#[test]
fn a_stalled_review_pauses_and_continues() {
    let bench = Bench::new("octet-fleet-stall");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 5));
    let mut turns = small_turns();
    turns["review_research#1"] = json!({
        "reply": verdict("changes_requested", &["The research misses fixture.txt."], &[])
    });
    turns["research#2"] = json!({"reply": "The repository holds a README."});
    turns["review_research#2"] = json!({
        "reply": verdict_previous("changes_requested", &[], &[("review_research/F1", "open")])
    });
    turns["research#3"] = json!({"reply": "The repository holds a README and fixture.txt."});
    turns["review_research#3"] = json!({"reply": approve(&["review_research/F1"])});
    bench.script(&turns);
    bench.run(&workflow, &[]).finish().expect_code(3);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::Stalled);
    assert_eq!(attempts(&records, "research"), 2);
    assert_eq!(attempts(&records, "implement"), 0);

    bench.resume(Some("continue"), &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Decision {
            decision: Decision::Continue
        }
    )));
    assert_eq!(attempts(&records, "research"), 3);
    assert_eq!(bench.plays_of("research#3").len(), 1);
}

/// A round limit: `max_rounds: 1` allows the first research and one
/// revision; a second rejection pauses. `rounds 2` raises the cap.
#[test]
fn a_round_limit_pauses_and_a_raised_cap_goes_on() {
    let bench = Bench::new("octet-fleet-rounds");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["review_research#1"] = json!({
        "reply": verdict("changes_requested", &["The research misses fixture.txt."], &[])
    });
    turns["research#2"] = json!({"reply": "The repository holds fixture.txt."});
    turns["review_research#2"] = json!({
        "reply": verdict(
            "changes_requested",
            &["The research misses the README."],
            &["review_research/F1"]
        )
    });
    turns["research#3"] = json!({"reply": "The repository holds a README and fixture.txt."});
    turns["review_research#3"] = json!({"reply": approve(&["review_research/F2"])});
    bench.script(&turns);
    bench.run(&workflow, &[]).finish().expect_code(3);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::RoundLimit);
    assert_eq!(attempts(&records, "research"), 2);
    assert_eq!(attempts(&records, "review_research"), 2);

    bench
        .resume_args(&["--decision", "rounds", "--rounds", "2"])
        .finish()
        .expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Decision {
            decision: Decision::Rounds { n: 2 }
        }
    )));
    assert_eq!(attempts(&records, "research"), 3);
}

/// An access violation: a reviewer writes a file in its clone. The work
/// tree is untouched; `retry` runs the review again in a fresh clone.
#[test]
fn a_reviewer_that_writes_is_an_access_violation() {
    let bench = Bench::new("octet-fleet-access");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["review_research#1"] = json!([
        {"write": {"notes.txt": "the reviewer's notes\n"}, "reply": approve(&[])},
        {"reply": approve(&[])},
    ]);
    bench.script(&turns);
    let base = bench.live_tree();
    bench.run(&workflow, &[]).finish().expect_code(3);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::AccessViolation);
    let reviewer = last_attempt_of(&records, "review_research").unwrap();
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::AttemptFailed {
                attempt,
                failure: Failure::AccessViolation { diff_stat },
            } if *attempt == reviewer && diff_stat.contains("notes.txt")
        )),
        "{records:#?}"
    );
    assert!(!bench.work.join("notes.txt").exists());
    assert_eq!(bench.live_tree(), base);

    bench.resume(Some("retry"), &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    assert_eq!(attempts(&records, "review_research"), 2);
    let plays = bench.plays_of("review_research#1");
    assert_eq!(plays.len(), 2);
    assert_ne!(plays[0]["cwd"], plays[1]["cwd"]);
    assert!(!bench.work.join("notes.txt").exists());
}

/// `UnstableTests → retry`: the command changes the tree on every pass until
/// a file outside the work tree says otherwise; `retry` starts the evidence
/// afresh on the current tip, its restart count reset.
#[test]
fn unstable_tests_retried_run_afresh_on_the_tip() {
    let bench = Bench::new("octet-fleet-unstable");
    let stable = bench.side("stable");
    let command = format!("[ -e '{}' ] || echo churn >> churn.txt", stable.display());
    let workflow = bench.workflow(&small(&[command.as_str()], 1));
    bench.script(&small_turns());
    bench.run(&workflow, &[]).finish().expect_code(3);
    let records = bench.records();
    assert_eq!(last_pause(&records), PauseReason::UnstableTests);
    let tester = last_attempt_of(&records, "test").unwrap();
    let tip = records
        .iter()
        .rev()
        .find_map(|record| match record {
            Record::EvidenceStarted {
                attempt,
                candidate,
                restart: 2,
            } if *attempt == tester => Some(*candidate),
            _ => None,
        })
        .expect("three passes");
    let paused_at = records.len();

    std::fs::write(&stable, "").unwrap();
    bench.resume(Some("retry"), &[]).finish().expect_code(0);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    assert!(matches!(
        &records[paused_at],
        Record::Decision {
            decision: Decision::Retry
        }
    ));
    // Afresh on the tip, restart 0; at most one more restart after it.
    let phases: Vec<(octet_fleet::record::CandidateId, u32)> = records[paused_at..]
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
    assert_eq!(phases.first(), Some(&(tip, 0)), "{records:#?}");
    assert!(
        phases.iter().all(|(_, restart)| *restart <= 1),
        "{phases:?}"
    );
    assert_eq!(bench.plays_of("test#1").len(), 1);
    bench.assert_passing_evidence_is_whole(&records, 1);
}

/// From the spec's "Plus" list: a transient retry that survives a restart
/// and waits out its backoff. The researcher hits a usage limit; Octet is
/// killed while it waits; the resumed run waits until the logged time
/// before the retry starts. It takes over a minute: the first backoff is
/// 60 s (spec, "Failures": 1, 5, then 15 minutes), and no setting shortens
/// it.
#[test]
fn a_transient_retry_survives_a_restart_and_waits_out_its_backoff() {
    let bench = Bench::new("octet-fleet-transient");
    let workflow = bench.workflow(&small(&["test -f feature.txt"], 1));
    let mut turns = small_turns();
    turns["research#1"] = json!([
        {"fail": true},
        {"reply": "The repository holds a README and fixture.txt."},
    ]);
    bench.script(&turns);
    let mut octet = bench.run(&workflow, &[]);
    let records = bench.await_record(&mut octet, "the retry", |record| {
        matches!(record, Record::Retry { .. })
    });
    let first = last_attempt_of(&records, "research").unwrap();
    let eligible = records
        .iter()
        .find_map(|record| match record {
            Record::Retry {
                attempt,
                count: 1,
                eligible_at,
                ..
            } if *attempt == first => Some(*eligible_at),
            _ => None,
        })
        .unwrap();
    assert!(
        records.iter().any(|record| matches!(
            record,
            Record::AttemptFailed {
                attempt,
                failure: Failure::Transient { eligible_at: Some(at), .. },
            } if *attempt == first && *at == eligible
        )),
        "{records:#?}"
    );
    // Killed while it waits, well before the retry is due.
    assert!(now_ms() < u128::from(eligible) * 1000);
    octet.sigkill();
    assert_eq!(attempts(&bench.records(), "research"), 1);

    let resumed = bench.resume(None, &[]).finish();
    resumed.expect_code(0);
    // The resumed run said it was waiting for the logged time.
    let waiting = format!(
        "{{\"at\":{eligible},\"attempt\":{},\"type\":\"retrying\"}}",
        first.0
    );
    assert!(resumed.stdout.contains(&waiting), "{}", resumed.stdout);
    let records = bench.records();
    assert!(matches!(records.last(), Some(Record::Completed { .. })));
    assert_eq!(attempts(&records, "research"), 2);
    // The retry is a new attempt, and it began no earlier than the logged
    // time.
    let plays = bench.plays_of("research#1");
    assert_eq!(plays.len(), 2);
    let began = u128::from(plays[1]["at_ms"].as_u64().unwrap());
    assert!(
        began >= u128::from(eligible) * 1000,
        "the retry began at {began} ms, before {eligible} s"
    );
}

/// The time now, in milliseconds since the Unix epoch.
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
