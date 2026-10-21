//! The live driver against the fake vendor: connection, turns, approvals,
//! modes, timeouts and shutdown for both protocols.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]
use octet_engine::live::{
    Command, Config, Engine, Event, ImageAttachment, Limits, Mode, Outcome, PROBE_LIMIT, ToolCall,
    ToolPhase, probe, spawn, spawn_with_limits,
};
use octet_testkit::scenario;
use std::{fmt::Write as _, time::Duration};
use tokio::{sync::mpsc, time::timeout};
fn config() -> Config {
    Config::new(
        Engine::CODEX,
        octet_testkit::protocol_child(),
        std::env::temp_dir(),
    )
}
/// The fake Claude, otherwise as `config()`.
fn claude() -> Config {
    let mut c = config();
    c.engine = Engine::CLAUDE;
    c
}
/// The fake opencode, otherwise as `config()`.
fn opencode() -> Config {
    let mut c = config();
    c.engine = Engine::OPENCODE;
    c
}
/// Shuts the session down and waits for it, failing instead of hanging.
async fn stop(handle: &octet_engine::live::Handle, task: tokio::task::JoinHandle<()>) {
    handle.shutdown();
    timeout(Duration::from_secs(3), task)
        .await
        .expect("the session did not stop")
        .unwrap();
}
/// Waits for a session that is ending by itself, failing instead of hanging.
async fn ended(task: tokio::task::JoinHandle<()>) {
    timeout(Duration::from_secs(3), task)
        .await
        .expect("the session did not stop")
        .unwrap();
}
/// The first error the session reports; it must report one before stopping.
async fn expect_error(events: &mut mpsc::Receiver<Event>) -> String {
    loop {
        match next(events).await {
            Event::Error(error) => return error,
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    }
}
async fn next(events: &mut mpsc::Receiver<Event>) -> Event {
    timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap()
}
/// A fake vendor from a shell script, in a directory that lives as long as
/// the returned guard.
fn script_vendor(name: &str, body: &str) -> (octet_testkit::TempDir, std::path::PathBuf) {
    let dir = octet_testkit::TempDir::new(name);
    let path = octet_testkit::write_script(dir.path(), "vendor", body);
    (dir, path)
}
#[tokio::test]
async fn codex_opening_the_session_before_the_handshake_is_an_error() {
    // Codex answers thread/start (id 2) before initialize (id 1).
    let (_dir, vendor) = script_vendor(
        "octet-live-out-of-order",
        "read line\n\
         echo '{\"id\":2,\"result\":{\"thread\":{\"id\":\"t1\"}}}'\n\
         echo '{\"id\":1,\"result\":{}}'\n\
         exec sleep 30\n",
    );
    let (_handle, mut events, task) =
        spawn(Config::new(Engine::CODEX, vendor, std::env::temp_dir()));
    loop {
        match next(&mut events).await {
            Event::Error(error) => {
                assert_eq!(error, "Unexpected protocol initialization order");
                break;
            }
            Event::Ready { session } if session != "t1" => {
                panic!("a second session opened: {session}")
            }
            _ => {}
        }
    }
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn a_repeated_claude_handshake_mid_turn_does_not_end_the_turn() {
    let init = r#"{"type":"control_response","response":{"subtype":"success","request_id":"octet-init","response":{}}}"#;
    let result = r#"{"type":"result","is_error":false,"result":"done","session_id":"s1"}"#;
    let (_dir, vendor) = script_vendor(
        "octet-live-repeated-init",
        &format!(
            "read line\necho '{init}'\nread line\necho '{init}'\necho '{result}'\nexec sleep 30\n"
        ),
    );
    let mut config = Config::new(Engine::CLAUDE, vendor, std::env::temp_dir());
    config.mode = Mode::Ask;
    let (handle, mut events, task) = spawn(config);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let finished = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Completed
            }
        ),
        "{finished:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn cancel_while_handshaken_is_cancelled() {
    // Codex answers initialize but never opens the session. The marker shows
    // the handshake was answered before the cancel, as the test's name says.
    let (dir, vendor) = script_vendor(
        "octet-live-handshaken",
        "read line\necho '{\"id\":1,\"result\":{}}'\ntouch \"$(dirname \"$0\")/answered\"\nexec sleep 30\n",
    );
    let (handle, mut events, task) =
        spawn(Config::new(Engine::CODEX, vendor, std::env::temp_dir()));
    let answered = dir.path().join("answered");
    // Polled without blocking: the driver runs on this test's runtime.
    timeout(Duration::from_secs(5), async {
        while !answered.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the handshake was never answered");
    // The frame is in the pipe; give the driver a moment to read it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.interrupt();
    loop {
        match next(&mut events).await {
            Event::Error(error) => {
                assert_eq!(error, "Connection cancelled");
                break;
            }
            Event::Ready { .. } => panic!("the session never opened"),
            _ => {}
        }
    }
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn cancel_while_starting_is_cancelled() {
    // A vendor that never answers the handshake.
    use std::os::unix::fs::PermissionsExt;
    let dir = octet_testkit::TempDir::new("octet-live-silent");
    std::fs::create_dir_all(dir.path()).unwrap();
    let silent = dir.path().join("silent-vendor");
    std::fs::write(&silent, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&silent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (handle, mut events, task) =
        spawn(Config::new(Engine::CODEX, silent, std::env::temp_dir()));
    handle.interrupt();
    loop {
        match next(&mut events).await {
            Event::Error(error) => {
                assert_eq!(error, "Connection cancelled");
                break;
            }
            Event::Ready { .. } => panic!("the silent vendor cannot be ready"),
            _ => {}
        }
    }
    assert!(matches!(next(&mut events).await, Event::Stopped));
    timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn live_driver_keeps_turns_separate_and_waits_for_terminal_after_usage() {
    let (handle, mut events, task) = spawn(config());
    assert!(matches!(next(&mut events).await, Event::Ready { .. }));
    for _ in 0..2 {
        handle.send(Command::Prompt("hello".into())).unwrap();
        let mut text = String::new();
        let mut usage = false;
        loop {
            match next(&mut events).await {
                Event::Text(t) => text.push_str(&t),
                Event::Usage(_) => usage = true,
                Event::Finished { outcome } => {
                    assert_eq!(outcome, Outcome::Completed);
                    break;
                }
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        assert_eq!(text, scenario::CODEX_REPLY);
        assert!(usage);
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn approvals_and_cancel_remain_routable_during_turn() {
    let (handle, mut events, task) = spawn(config());
    next(&mut events).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let id = loop {
        if let Event::Approval { id, .. } = next(&mut events).await {
            break id;
        }
    };
    handle.send(Command::Answer { id, allow: false }).unwrap();
    let mut denied = false;
    loop {
        match next(&mut events).await {
            Event::Text(t) => denied |= t == "decline",
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    assert!(denied);
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    while !matches!(next(&mut events).await, Event::Started) {}
    handle.interrupt();
    loop {
        match next(&mut events).await {
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Interrupted);
                break;
            }
            Event::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    handle.send(Command::Answer { id, allow: true }).unwrap();
    assert!(matches!(next(&mut events).await, Event::Notice(_)));
    stop(&handle, task).await;
}

#[tokio::test]
async fn claude_stream_does_not_duplicate_final_assistant_message() {
    let mut config = config();
    config.engine = Engine::CLAUDE;
    let (handle, mut events, task) = spawn(config);
    assert!(matches!(next(&mut events).await, Event::Ready { .. }));
    for _ in 0..2 {
        handle.send(Command::Prompt("hello".into())).unwrap();
        let mut text = String::new();
        loop {
            match next(&mut events).await {
                Event::Text(t) => text.push_str(&t),
                Event::Finished { outcome } => {
                    assert_eq!(outcome, Outcome::Completed);
                    break;
                }
                Event::Error(e) => panic!("{e}"),
                _ => {}
            }
        }
        assert_eq!(text, scenario::CLAUDE_REPLY);
    }
    stop(&handle, task).await;
}

#[tokio::test]
async fn codex_discovers_all_pages_and_uses_model_not_picker_id() {
    let (handle, mut events, task) = spawn(config());
    let mut confirmed = None;
    loop {
        match next(&mut events).await {
            Event::ModelSelected(id) => confirmed = Some(id),
            Event::Models(models) if models.len() == 2 => {
                assert_eq!(models[0].selection, "fixture");
                assert_eq!(models[0].id.as_deref(), Some("fixture"));
                assert_eq!(models[0].name, "Fixture Model");
                assert_eq!(models[1].selection, "other-full-id");
                assert_eq!(confirmed.as_deref(), Some("fixture"));
                break;
            }
            Event::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    stop(&handle, task).await;
}

#[tokio::test]
async fn claude_catalog_alias_is_distinct_from_confirmed_session_model() {
    let mut cfg = config();
    cfg.engine = Engine::CLAUDE;
    cfg.model = Some("sonnet".into());
    let (handle, mut events, task) = spawn(cfg);
    loop {
        if let Event::Models(models) = next(&mut events).await {
            assert_eq!(models[0].selection, "sonnet");
            assert_eq!(models[0].id.as_deref(), Some("claude-fixture-full-id"));
            assert_eq!(models[0].name, "Fixture Sonnet");
            break;
        }
    }
    handle.send(Command::Prompt("hello".into())).unwrap();
    loop {
        if let Event::ModelSelected(id) = next(&mut events).await {
            assert_eq!(id, "claude-fixture-full-id");
            break;
        }
    }
    stop(&handle, task).await;
}

#[tokio::test]
async fn provider_receives_wire_prompt_while_transcript_keeps_user_facing_text() {
    let (handle, mut events, task) = spawn(config());
    while !matches!(next(&mut events).await, Event::Ready { .. }) {}
    handle
        .send(Command::PromptWithDisplay {
            wire: scenario::HOLD.into(),
            display: "hello".into(),
            images: Vec::new(),
        })
        .unwrap();
    let mut seen_user = false;
    loop {
        match next(&mut events).await {
            Event::User(text) => {
                assert_eq!(text, "hello");
                seen_user = true;
            }
            Event::Started => {
                handle.interrupt();
            }
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Interrupted);
                break;
            }
            Event::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    assert!(seen_user);
    stop(&handle, task).await;
}

async fn wait_for(events: &mut mpsc::Receiver<Event>, wanted: impl Fn(&Event) -> bool) -> Event {
    loop {
        let event = next(events).await;
        if let Event::Error(error) = &event {
            panic!("{error}");
        }
        if wanted(&event) {
            return event;
        }
    }
}
async fn turn_text(events: &mut mpsc::Receiver<Event>) -> String {
    let mut text = String::new();
    loop {
        match next(events).await {
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => return text,
            Event::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
}
#[tokio::test]
async fn codex_launches_and_turns_with_the_configured_mode() {
    let mut c = config();
    c.mode = Mode::Auto;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Auto))).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["thread"]["sandbox"], "workspace-write");
    assert_eq!(echo["thread"]["approvalPolicy"], "on-request");
    assert_eq!(echo["thread"]["approvalsReviewer"], "auto_review");
    assert_eq!(echo["turn"]["approvalPolicy"], "on-request");
    assert_eq!(echo["turn"]["approvalsReviewer"], "auto_review");
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_launches_with_the_mapped_permission_flag() {
    let mut c = claude();
    c.mode = Mode::AcceptEdits;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    handle.send(Command::Prompt(scenario::ARGV.into())).unwrap();
    let argv = turn_text(&mut events).await;
    assert!(argv.contains("--permission-mode acceptEdits"), "{argv}");
    assert!(!argv.contains("--permission-mode default"), "{argv}");
    assert!(!argv.contains("dangerously"), "{argv}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_live_switch_applies_to_the_next_turn() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Auto))).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["thread"]["approvalsReviewer"], "user");
    assert_eq!(echo["turn"]["approvalPolicy"], "on-request");
    assert_eq!(echo["turn"]["approvalsReviewer"], "auto_review");
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_live_switch_uses_set_permission_mode() {
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Auto))).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_switch_while_steering_reaches_the_first_new_turn_and_stays() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::Prompt(scenario::HOLD_MARKED.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Text(t) if t == "named")).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Auto))).await;
    let notice = next(&mut events).await;
    assert!(
        matches!(notice, Event::Notice(t) if t.contains("steering keep their previous permissions"))
    );
    for text in ["first follow-up", "second follow-up"] {
        handle.send(Command::Steer(text.into())).unwrap();
        wait_for(
            &mut events,
            |e| matches!(e, Event::Text(t) if t == &format!("steered:{text}")),
        )
        .await;
    }
    handle.interrupt();
    wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    for _ in 0..2 {
        handle
            .send(Command::Prompt(scenario::PARAMS.into()))
            .unwrap();
        let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
        assert_eq!(echo["turn"]["approvalPolicy"], "on-request");
        assert_eq!(echo["turn"]["approvalsReviewer"], "auto_review");
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_refusal_keeps_the_previous_mode() {
    let mut c = claude();
    c.model = Some(scenario::REJECT_MODE.into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    let notice = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&notice, Event::Notice(text) if text.contains("refused")),
        "{notice:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Ask)
    ));
    stop(&handle, task).await;
}
#[tokio::test]
async fn driver_refuses_live_full_access() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::FullAccess)).unwrap();
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("/mode")),
        "{event:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Ask)
    ));
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_header_follows_the_mode_claude_reports() {
    let mut c = claude();
    c.model = Some(scenario::REPORT_AUTO.into());
    let (handle, mut events, task) = spawn(c);
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("reports auto")),
        "{event:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Auto)
    ));
    stop(&handle, task).await;
}
#[tokio::test]
async fn unmapped_claude_mode_keeps_the_requested_mode_with_a_notice() {
    let mut c = claude();
    c.model = Some(scenario::REPORT_PLAN.into());
    let (handle, mut events, task) = spawn(c);
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("plan")),
        "{event:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Ask)
    ));
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_header_follows_a_stricter_reported_policy() {
    let mut c = config();
    c.mode = Mode::Auto;
    c.model = Some(scenario::REPORT_STRICTER.into());
    let (handle, mut events, task) = spawn(c);
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(text) if text.contains("reports"))
            || matches!(e, Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("reports ask")),
        "{event:?}"
    );
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["turn"]["approvalPolicy"], "untrusted");
    stop(&handle, task).await;
}
/// Real limits shortened so timeout paths run in seconds.
fn quick() -> Limits {
    Limits {
        connect: Duration::from_secs(2),
        turn_idle: Duration::from_secs(1),
        mode_confirm: Duration::from_secs(1),
        ..Limits::default()
    }
}
/// Like `wait_for`, allowing `seconds` instead of five.
async fn wait_for_within(
    events: &mut mpsc::Receiver<Event>,
    seconds: u64,
    wanted: impl Fn(&Event) -> bool + Send,
) -> Event {
    timeout(Duration::from_secs(seconds), async {
        loop {
            let event = events.recv().await.expect("driver stopped");
            if let Event::Error(error) = &event {
                panic!("{error}");
            }
            if wanted(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event did not arrive in time")
}
#[tokio::test]
async fn unconfirmed_claude_switch_times_out_then_applies_a_late_confirmation() {
    let mut c = claude();
    c.model = Some(scenario::LATE_MODE.into());
    let (handle, mut events, task) = spawn_with_limits(c, quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    // Idle past the connect deadline: the mode timer firing later must not be
    // mistaken for an expired turn.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    let event = wait_for_within(&mut events, 12, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("has not confirmed") && text.contains("until it does")),
        "{event:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Ask)
    ));
    let event = wait_for_within(&mut events, 5, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("late")),
        "{event:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Auto)
    ));
    stop(&handle, task).await;
}
#[tokio::test]
async fn demo_mode_switch_during_approval_keeps_the_dialog_open() {
    let mut c = config();
    c.engine = Engine::DEMO;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::Prompt("/approval-demo".into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(_) | Event::ApprovalClosed(_))
    })
    .await;
    assert!(matches!(event, Event::ModeChanged(Mode::Auto)), "{event:?}");
    handle.send(Command::Answer { id: 1, allow: true }).unwrap();
    assert!(turn_text(&mut events).await.starts_with("Approved."));
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_switch_sends_the_vendor_mode_name() {
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    for (mode, ()) in [(Mode::AcceptEdits, ()), (Mode::Auto, ()), (Mode::Ask, ())] {
        handle.send(Command::SetMode(mode)).unwrap();
        wait_for(
            &mut events,
            |e| matches!(e, Event::ModeChanged(m) if *m == mode),
        )
        .await;
    }
    handle
        .send(Command::Prompt(scenario::MODES.into()))
        .unwrap();
    assert_eq!(turn_text(&mut events).await, "acceptEdits,auto,default");
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_switch_applies_in_the_middle_of_a_turn() {
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    let event = wait_for(&mut events, |e| {
        matches!(
            e,
            Event::ModeChanged(_) | Event::Finished { .. } | Event::Notice(_)
        )
    })
    .await;
    assert!(matches!(event, Event::ModeChanged(Mode::Auto)), "{event:?}");
    handle.interrupt();
    let event = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(matches!(event, Event::Finished { outcome } if outcome == Outcome::Interrupted));
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_resume_carries_the_mode() {
    let mut c = config();
    c.resume = Some("fixture-thread".into());
    c.mode = Mode::FullAccess;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::FullAccess))
    })
    .await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["thread"]["method"], "thread/resume");
    assert_eq!(echo["thread"]["threadId"], "fixture-thread");
    assert_eq!(echo["thread"]["sandbox"], "danger-full-access");
    assert_eq!(echo["thread"]["approvalPolicy"], "never");
    assert_eq!(echo["turn"]["approvalPolicy"], "never");
    stop(&handle, task).await;
}
#[tokio::test]
async fn idle_switch_long_after_connect_confirms_promptly() {
    let c = claude();
    let (handle, mut events, task) = spawn_with_limits(c, quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    // Past the connect deadline, which is stale once idle.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    let event = wait_for_within(&mut events, 12, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    // The first event being the confirmation, not a timeout notice, is the proof.
    assert!(matches!(event, Event::ModeChanged(Mode::Auto)), "{event:?}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn every_timed_out_switch_can_still_be_confirmed_late() {
    let mut c = claude();
    c.model = Some(scenario::HANG_MODE.into());
    let (handle, mut events, task) = spawn_with_limits(c, quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    for target in [Mode::AcceptEdits, Mode::Auto] {
        handle.send(Command::SetMode(target)).unwrap();
        wait_for_within(
            &mut events,
            12,
            |e| matches!(e, Event::Notice(t) if t.contains("has not confirmed")),
        )
        .await;
        assert!(matches!(
            next(&mut events).await,
            Event::ModeChanged(Mode::Ask)
        ));
    }
    // Claude accepts the first request late and refuses the second.
    handle
        .send(Command::Prompt(scenario::FLUSH.into()))
        .unwrap();
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn unmapped_switch_reply_keeps_the_target_with_a_notice() {
    let mut c = claude();
    c.model = Some(scenario::ODD_MODE.into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&event, Event::Notice(text) if text.contains("plan")),
        "{event:?}"
    );
    assert!(matches!(
        next(&mut events).await,
        Event::ModeChanged(Mode::Auto)
    ));
    stop(&handle, task).await;
}
#[tokio::test]
async fn vendor_exit_reports_its_stderr() {
    let mut c = claude();
    c.model = Some(scenario::DIE_STDERR.into());
    let (_handle, mut events, task) = spawn(c);
    let error = expect_error(&mut events).await;
    assert!(
        error.contains("No conversation found with session ID: fixture"),
        "{error}"
    );
    ended(task).await;
}
#[tokio::test]
async fn codex_thread_refusal_reports_the_vendor_message() {
    let mut c = config();
    c.model = Some(scenario::REFUSE_THREAD.into());
    let (_handle, mut events, task) = spawn(c);
    let error = expect_error(&mut events).await;
    assert!(
        error.contains("no rollout found for thread id fixture"),
        "{error}"
    );
    ended(task).await;
}
#[tokio::test]
async fn large_tool_item_is_bounded_and_the_turn_completes() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::BIGTOOL.into()))
        .unwrap();
    let mut tool_bytes = 0;
    let mut tool = String::new();
    loop {
        match next(&mut events).await {
            Event::Tool(call) => {
                tool_bytes += call.detail.len();
                tool = call.detail;
            }
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Completed);
                break;
            }
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(tool_bytes > 0 && tool_bytes <= 40 * 1024, "{tool_bytes}");
    // The cut must keep what the command was and how it ended, not only output.
    let head: String = tool.chars().take(200).collect();
    assert!(
        head.contains("cat big") && head.contains("exit 0"),
        "{head}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn turn_errors_show_the_vendor_message_not_raw_json() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    for (prompt, expected) in [
        (
            scenario::FAIL,
            "You've hit your usage limit. (usageLimitExceeded)",
        ),
        (scenario::FAIL_NESTED, "The model is not supported."),
    ] {
        handle.send(Command::Prompt(prompt.into())).unwrap();
        let error = loop {
            match next(&mut events).await {
                Event::Error(error) => break error,
                Event::Finished { .. } => panic!("finished without an error"),
                _ => {}
            }
        };
        assert_eq!(error, expected);
        assert!(
            matches!(next(&mut events).await, Event::Finished { outcome } if outcome == Outcome::Failed)
        );
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_result_errors_are_shown() {
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::ERRORS.into()))
        .unwrap();
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::Finished { .. } => panic!("finished without an error"),
            _ => {}
        }
    };
    assert_eq!(error, "Fixture failure detail");
    stop(&handle, task).await;
}
async fn claude_tool_calls(prompt: &str) -> Vec<ToolCall> {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(prompt.into())).unwrap();
    let mut tools = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Tool(call) => tools.push(call),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    stop(&handle, task).await;
    tools
}
#[tokio::test]
async fn claude_tool_starts_with_its_target_and_ends_with_its_result() {
    let tools = claude_tool_calls(scenario::TOOL).await;
    assert_eq!(tools.len(), 2, "{tools:?}");
    assert_eq!(
        (
            tools[0].phase,
            tools[0].name.as_str(),
            tools[0].target.as_str()
        ),
        (ToolPhase::Running, "Bash", "echo fixture")
    );
    assert_eq!(
        (tools[1].phase, tools[1].id.as_str(), tools[1].name.as_str()),
        (ToolPhase::Done, "toolu_1", "Bash")
    );
    assert_eq!(tools[1].target, "echo fixture");
    assert_eq!(tools[1].detail, "fixture\n");
}
#[tokio::test]
async fn claude_tool_error_is_a_failed_call() {
    let tools = claude_tool_calls(scenario::TOOL_FAIL).await;
    assert_eq!(
        tools.last().map(|t| (t.phase, t.detail.as_str())),
        Some((ToolPhase::Failed, "exit 1"))
    );
}
#[tokio::test]
async fn active_turn_outlives_the_idle_limit() {
    let (handle, mut events, task) = spawn_with_limits(config(), quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    // The fixture streams for about 2.4 seconds; the idle limit is 1 second.
    handle.send(Command::Prompt(scenario::SLOW.into())).unwrap();
    let event = wait_for_within(&mut events, 6, |e| matches!(e, Event::Finished { .. })).await;
    assert!(matches!(event, Event::Finished { outcome } if outcome == Outcome::Completed));
    stop(&handle, task).await;
}
#[tokio::test]
async fn silent_turn_is_stopped_at_the_idle_limit() {
    let (handle, mut events, task) = spawn_with_limits(config(), quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    let error = expect_error(&mut events).await;
    assert!(error.contains("sent nothing for 1 second"), "{error}");
    let _ = handle;
    ended(task).await;
}
#[tokio::test]
async fn other_thread_chatter_does_not_keep_a_silent_turn_alive() {
    let (handle, mut events, task) = spawn_with_limits(config(), quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let sent = tokio::time::Instant::now();
    // Another thread emits frames for about 2.4 seconds; ours is silent.
    handle
        .send(Command::Prompt(scenario::CHATTER.into()))
        .unwrap();
    let error = expect_error(&mut events).await;
    assert!(error.contains("sent nothing for 1 second"), "{error}");
    // The 1 s idle limit fired. Had the chatter reset it, the error would
    // come only after the chatter's 2.4 s plus the limit: 3.4 s at least.
    assert!(
        sent.elapsed() < Duration::from_secs(3),
        "{:?}",
        sent.elapsed()
    );
    let _ = handle;
    ended(task).await;
}
#[tokio::test]
async fn waiting_for_the_user_is_not_vendor_silence() {
    let (handle, mut events, task) = spawn_with_limits(config(), quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    // Longer than the 1-second idle limit: the vendor is waiting on us.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    handle.send(Command::Answer { id, allow: true }).unwrap();
    let event = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(matches!(event, Event::Finished { outcome } if outcome == Outcome::Completed));
    stop(&handle, task).await;
}

#[tokio::test]
async fn spawn_uses_the_configured_approval_window() {
    let config = Config {
        approval_timeout: Duration::from_millis(300),
        ..Config::new(Engine::DEMO, "demo", std::env::temp_dir())
    };
    let (handle, mut events, task) = spawn(config);
    handle
        .send(Command::Prompt("/approval-demo".into()))
        .unwrap();
    loop {
        match next(&mut events).await {
            Event::ApprovalClosed(_) => break,
            Event::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    stop(&handle, task).await;
}

#[tokio::test]
async fn codex_steer_adds_to_the_running_turn() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle
        .send(Command::Steer("look at tests too".into()))
        .unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::User(t) if t == "[steer] look at tests too"),
    )
    .await;
    wait_for(
        &mut events,
        |e| matches!(e, Event::Text(t) if t == "steered:look at tests too"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn steer_without_a_turn_is_refused() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Steer("anything".into())).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(n) if n.contains("No turn is running")),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_sends_effort_on_each_turn_and_takes_changes_live() {
    let mut c = config();
    c.effort = Some("high".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    assert!(turn_text(&mut events).await.contains("\"effort\":\"high\""));
    handle.send(Command::SetEffort(Some("low".into()))).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(n) if n.contains("low")),
    )
    .await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    assert!(turn_text(&mut events).await.contains("\"effort\":\"low\""));
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_launches_with_effort() {
    let mut c = claude();
    c.effort = Some("max".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt(scenario::ARGV.into())).unwrap();
    assert!(turn_text(&mut events).await.contains("--effort max"));
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_fork_opens_a_new_thread() {
    let mut c = config();
    c.resume = Some("fixture-thread".into());
    c.fork = true;
    let (handle, mut events, task) = spawn(c);
    wait_for(
        &mut events,
        |e| matches!(e, Event::Ready { session } if session == "forked-thread"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_failed_command_is_a_failed_call() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::TOOL_FAIL.into()))
        .unwrap();
    let mut calls = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Tool(call) => calls.push(call),
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    assert_eq!(
        calls.iter().map(|c| c.phase).collect::<Vec<_>>(),
        [ToolPhase::Running, ToolPhase::Failed]
    );
    assert_eq!(calls[1].target, "false");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_file_approval_names_the_items_files() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::FILE_APPROVAL.into()))
        .unwrap();
    let Event::Approval { id, summary, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(summary.action, "Edit 2 files");
    assert_eq!(summary.files, ["a.rs", "b.rs"]);
    handle.send(Command::Answer { id, allow: false }).unwrap();
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_compact_runs_as_a_turn() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Compact).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::User(t) if t == "/compact"),
    )
    .await;
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    wait_for(&mut events, |e| {
        matches!(
            e,
            Event::Finished {
                outcome: Outcome::Completed
            }
        )
    })
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_fork_passes_fork_session() {
    let mut c = claude();
    c.resume = Some("claude-fixture".into());
    c.fork = true;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt(scenario::ARGV.into())).unwrap();
    let argv = turn_text(&mut events).await;
    assert!(
        argv.contains("--resume claude-fixture --fork-session"),
        "{argv}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_compact_sends_the_command() {
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Compact).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::User(t) if t == "/compact"),
    )
    .await;
    assert_eq!(turn_text(&mut events).await, "Compacted");
    stop(&handle, task).await;
}
/// A prompt carrying one image file written with `bytes`.
fn image_prompt(dir: &octet_testkit::TempDir, bytes: &[u8]) -> (Command, ImageAttachment) {
    std::fs::create_dir_all(dir.path()).unwrap();
    let path = dir.path().join("shot.png");
    std::fs::write(&path, bytes).unwrap();
    let image = ImageAttachment::open(&path).unwrap();
    let command = Command::PromptWithDisplay {
        wire: "look".into(),
        display: "look\n[+ image shot.png]".into(),
        images: vec![image.clone()],
    };
    (command, image)
}
#[tokio::test]
async fn codex_receives_local_image_paths() {
    let dir = octet_testkit::TempDir::new("octet-image");
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    let (command, image) = image_prompt(&dir, b"fake png");
    handle.send(command).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::User(t) if t == "look\n[+ image shot.png]"),
    )
    .await;
    assert_eq!(
        turn_text(&mut events).await,
        format!("images:{}", image.path.display())
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_receives_base64_image_blocks() {
    let dir = octet_testkit::TempDir::new("octet-image");
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    // "abc" is "YWJj" in base64: four bytes on the wire.
    let (command, _) = image_prompt(&dir, b"abc");
    handle.send(command).unwrap();
    assert_eq!(turn_text(&mut events).await, "image:image/png:4");
    stop(&handle, task).await;
}
#[tokio::test]
async fn an_image_gone_at_send_time_fails_the_turn() {
    let dir = octet_testkit::TempDir::new("octet-image");
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    let (command, image) = image_prompt(&dir, b"abc");
    std::fs::remove_file(&image.path).unwrap();
    handle.send(command).unwrap();
    let mut error = None;
    loop {
        match next(&mut events).await {
            Event::Error(text) => error = Some(text),
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Failed);
                break;
            }
            _ => {}
        }
    }
    assert!(error.unwrap().contains("shot.png"));
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_fork_names_the_new_session_only_after_the_first_prompt() {
    let mut c = claude();
    c.resume = Some("claude-fixture".into());
    c.fork = true;
    let (handle, mut events, task) = spawn(c);
    // Until Claude names the fork, there is no session to resume: reporting
    // the original would send the next reconnect back into it.
    let ready = wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    assert!(
        matches!(ready, Event::Ready { ref session } if session.is_empty()),
        "{ready:?}"
    );
    handle.send(Command::Prompt("hello".into())).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Ready { session } if session == "claude-forked"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_rejected_steer_is_reported_with_its_text() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::HOLD_MARKED.into()))
        .unwrap();
    // The marker comes once Codex has named the turn, so the steer goes out.
    wait_for(&mut events, |e| matches!(e, Event::Text(t) if t == "named")).await;
    handle.send(Command::Steer("reject".into())).unwrap();
    wait_for(&mut events, |e| {
        matches!(e, Event::Notice(t) if t.contains("Codex did not take the steer") && t.contains("reject"))
    })
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_images_too_large_together_fail_the_turn_not_the_session() {
    let dir = octet_testkit::TempDir::new("octet-image-big");
    std::fs::create_dir_all(dir.path()).unwrap();
    let mut images = Vec::new();
    for name in ["a.png", "b.png"] {
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .unwrap()
            .set_len(3 * 1024 * 1024)
            .unwrap();
        images.push(ImageAttachment::open(&path).unwrap());
    }
    let c = claude();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::PromptWithDisplay {
            wire: "look".into(),
            display: "look".into(),
            images,
        })
        .unwrap();
    let mut error = String::new();
    loop {
        match next(&mut events).await {
            Event::Error(text) => error = text,
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Failed);
                break;
            }
            Event::Stopped => panic!("the session stopped: {error}"),
            _ => {}
        }
    }
    assert!(error.contains("together"), "{error}");
    // The session is still usable.
    handle.send(Command::Prompt("hello".into())).unwrap();
    assert_eq!(turn_text(&mut events).await, scenario::CLAUDE_REPLY);
    stop(&handle, task).await;
}
#[tokio::test]
async fn steer_while_interrupting_names_the_text_it_did_not_send() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.interrupt();
    handle.send(Command::Steer("too late".into())).unwrap();
    wait_for(&mut events, |e| {
        matches!(e, Event::Notice(t) if t.contains("The turn is stopping") && t.contains("too late"))
    })
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_prompt_cancelled_before_it_starts_is_not_run() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    // Esc in the same instant as the send: the cancel must still win.
    handle.send(Command::Prompt("hello".into())).unwrap();
    handle.interrupt();
    let outcome = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(
        matches!(
            outcome,
            Event::Finished {
                outcome: Outcome::Interrupted
            }
        ),
        "{outcome:?}"
    );
    // A later prompt is not affected.
    handle.send(Command::Prompt("hello".into())).unwrap();
    assert_eq!(turn_text(&mut events).await, scenario::CODEX_REPLY);
    stop(&handle, task).await;
}
#[tokio::test]
async fn the_demo_also_cancels_a_prompt_that_had_not_started() {
    let mut c = config();
    c.engine = Engine::DEMO;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    handle.interrupt();
    let outcome = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(
        matches!(
            outcome,
            Event::Finished {
                outcome: Outcome::Interrupted
            }
        ),
        "{outcome:?}"
    );
    stop(&handle, task).await;
}
/// Runs `prompt` until its first approval, answers it, and returns the
/// turn's text.
async fn answer_first_approval(engine: Config, prompt: &str, allow: bool) -> String {
    let (handle, mut events, task) = spawn(engine);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt(prompt.into())).unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    handle.send(Command::Answer { id, allow }).unwrap();
    let text = turn_text(&mut events).await;
    stop(&handle, task).await;
    text
}
#[tokio::test]
async fn claude_approval_allowed_passes_the_input_back() {
    assert_eq!(
        answer_first_approval(claude(), scenario::APPROVAL, true).await,
        "allow:echo fixture"
    );
}
#[tokio::test]
async fn claude_approval_denied() {
    assert_eq!(
        answer_first_approval(claude(), scenario::APPROVAL, false).await,
        "deny:The user denied this request"
    );
}
/// Sends `approvals-9`: the ninth request is denied at the cap, and the
/// eight shown can still be answered. How many were shown.
async fn approvals_before_the_cap(engine: Config) -> usize {
    let claude = engine.engine == Engine::CLAUDE;
    let (allow, deny) = if claude {
        ("allow", "deny:Too many approvals are waiting; denied")
    } else {
        ("accept", "decline")
    };
    let (handle, mut events, task) = spawn(engine);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVALS_9.into()))
        .unwrap();
    let mut shown = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Approval { id, .. } => shown.push(id),
            Event::Notice(text) if text.contains("Too many approvals are waiting") => break,
            Event::Finished { .. } | Event::Stopped => panic!("no cap notice"),
            _ => {}
        }
    }
    // Only the ninth request was denied; the eight shown still wait.
    wait_for(
        &mut events,
        |e| matches!(e, Event::Text(t) if *t == format!("answered cap-9:{deny}")),
    )
    .await;
    handle
        .send(Command::Answer {
            id: shown[0],
            allow: true,
        })
        .unwrap();
    let echoed = format!("answered cap-1:{allow}");
    wait_for(&mut events, |e| matches!(e, Event::Text(t) if *t == echoed)).await;
    handle.interrupt();
    stop(&handle, task).await;
    shown.len()
}
#[tokio::test]
async fn approvals_over_the_cap_are_denied_with_a_notice() {
    assert_eq!(approvals_before_the_cap(config()).await, 8);
    assert_eq!(approvals_before_the_cap(claude()).await, 8);
}
#[tokio::test]
async fn an_unanswered_approval_times_out_and_is_denied() {
    for (mut engine, denied) in [
        (config(), "decline"),
        (claude(), "deny:No answer in time; denied"),
    ] {
        engine.approval_timeout = Duration::from_millis(300);
        let (handle, mut events, task) = spawn_with_limits(engine, quick());
        wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
        handle
            .send(Command::Prompt(scenario::APPROVAL.into()))
            .unwrap();
        let Event::Approval { id, .. } =
            wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
        else {
            unreachable!()
        };
        wait_for(
            &mut events,
            |e| matches!(e, Event::ApprovalClosed(closed) if *closed == id),
        )
        .await;
        assert_eq!(turn_text(&mut events).await, denied);
        stop(&handle, task).await;
    }
}
#[tokio::test]
async fn claude_result_without_is_error_is_failed_once() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::NO_IS_ERROR.into()))
        .unwrap();
    let mut errors = 0;
    let outcome = loop {
        match next(&mut events).await {
            Event::Error(_) => errors += 1,
            Event::Finished { outcome } => break outcome,
            _ => {}
        }
    };
    assert_eq!((errors, outcome), (1, Outcome::Failed));
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_steer_held_for_a_cancelled_turn_is_reported() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::LATE_START.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    // Codex has not named the turn yet, so the steer is held.
    handle.send(Command::Steer("held text".into())).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::User(t) if t == "[steer] held text"),
    )
    .await;
    handle.interrupt();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(t) if t == "This steer was not sent: held text"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_steer_held_for_a_refused_turn_is_reported() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::REFUSE_START.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.send(Command::Steer("held text".into())).unwrap();
    let mut reported = false;
    let outcome = loop {
        match next(&mut events).await {
            Event::Notice(t) if t == "This steer was not sent: held text" => reported = true,
            Event::Finished { outcome } => break outcome,
            _ => {}
        }
    };
    assert_eq!(outcome, Outcome::Failed);
    assert!(reported, "the held steer was dropped silently");
    stop(&handle, task).await;
}
#[tokio::test]
async fn held_steers_are_bounded() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::LATE_START.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    for n in 1..=9 {
        handle.send(Command::Steer(format!("steer {n}"))).unwrap();
    }
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(t) if t == "This steer was not sent: steer 1"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_prompt_sent_during_the_demo_dialog_is_refused_and_counted() {
    let mut c = config();
    c.engine = Engine::DEMO;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt("/approval-demo".into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await;
    // Refused while the dialog waits, and still counted as a turn sent.
    handle.send(Command::Prompt("too soon".into())).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(t) if t.starts_with("Wait for the current operation")),
    )
    .await;
    handle.interrupt();
    wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    // A prompt sent after the cancel runs.
    handle.send(Command::Prompt("after".into())).unwrap();
    let outcome = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(
        matches!(
            outcome,
            Event::Finished {
                outcome: Outcome::Completed
            }
        ),
        "{outcome:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_cancel_after_a_demo_turn_does_not_stop_the_next_one() {
    let mut c = config();
    c.engine = Engine::DEMO;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    // The race needs the cancel and the next prompt in the same poll, so
    // repeat it: before the fix about two rounds in five failed, so six
    // rounds catch it about 95% of the time (each demo turn takes ~1 s).
    for round in 0..6 {
        handle.send(Command::Prompt("hello".into())).unwrap();
        wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
        // Esc on an idle session, then a new prompt: the cancel is old news.
        handle.interrupt();
        handle.send(Command::Prompt("hello".into())).unwrap();
        let outcome = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
        assert!(
            matches!(
                outcome,
                Event::Finished {
                    outcome: Outcome::Completed
                }
            ),
            "round {round}: {outcome:?}"
        );
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_cancel_request_closes_the_approval() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL_CANCEL.into()))
        .unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    // Claude cancels the request while its turn is still running.
    wait_for(
        &mut events,
        |e| matches!(e, Event::ApprovalClosed(closed) if *closed == id),
    )
    .await;
    // A late answer is refused here and never reaches Claude.
    handle.send(Command::Answer { id, allow: true }).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(t) if t == "That approval is no longer active"),
    )
    .await;
    handle.interrupt();
    assert!(
        !turn_text(&mut events)
            .await
            .contains("answered a cancelled request")
    );
    stop(&handle, task).await;
}

/// Every event up to the end of the turn (or the session).
async fn until_finished(events: &mut mpsc::Receiver<Event>) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let event = next(events).await;
        let done = matches!(event, Event::Finished { .. } | Event::Stopped);
        seen.push(event);
        if done {
            return seen;
        }
    }
}
fn text_of(seen: &[Event]) -> String {
    seen.iter()
        .filter_map(|event| match event {
            Event::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_burst_of_deltas_during_a_stall_is_delivered() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::DELTA_BURST.into()))
        .unwrap();
    // The interface stalls (a slow disk, a busy screen) while Claude streams.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let seen = until_finished(&mut events).await;
    assert_eq!(text_of(&seen), "x".repeat(200), "{:?}", seen.last());
    assert!(matches!(
        seen.last(),
        Some(Event::Finished {
            outcome: Outcome::Completed
        })
    ));
    stop(&handle, task).await;
}

#[tokio::test]
async fn claude_subagent_text_is_not_the_reply() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::SUBAGENT.into()))
        .unwrap();
    let seen = until_finished(&mut events).await;
    assert_eq!(text_of(&seen), "MAIN");
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::ModelSelected(m) if m == "claude-sub")),
        "{seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Tool(t) if t.name == "Read")),
        "the subagent's tool activity still shows: {seen:?}"
    );
    stop(&handle, task).await;
}

#[tokio::test]
async fn a_huge_approval_timeout_does_not_panic() {
    let mut config = claude();
    config.approval_timeout = Duration::MAX;
    let (handle, mut events, task) = spawn(config);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await;
    handle.interrupt();
    let seen = until_finished(&mut events).await;
    assert!(
        matches!(seen.last(), Some(Event::Finished { .. })),
        "{seen:?}"
    );
    stop(&handle, task).await;
}

#[tokio::test]
async fn a_second_cancel_keeps_the_interrupt_deadline() {
    let limits = Limits {
        interrupt: Duration::from_secs(2),
        ..Limits::default()
    };
    let (handle, mut events, task) = spawn_with_limits(config(), limits);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::IGNORE_INTERRUPT.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    // Let Codex name the turn, so the interrupt reaches it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let first = std::time::Instant::now();
    handle.interrupt();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    handle.interrupt();
    let error = expect_error(&mut events).await;
    assert!(error.contains("did not stop within"), "{error}");
    let waited = first.elapsed();
    // The deadline runs from the first cancel (2 s), not the second (3.2 s).
    assert!(waited < Duration::from_millis(2800), "{waited:?}");
    ended(task).await;
}

#[tokio::test]
async fn an_unsafe_session_id_is_ignored() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::ODD_SESSION.into()))
        .unwrap();
    let seen = until_finished(&mut events).await;
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::Ready { session } if session.contains('\u{1b}'))),
        "{seen:?}"
    );
    stop(&handle, task).await;
}

#[tokio::test]
async fn codex_cuts_huge_vendor_strings() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::ODD_STRINGS.into()))
        .unwrap();
    let seen = until_finished(&mut events).await;
    let declined = seen
        .iter()
        .find_map(|e| match e {
            Event::Notice(n) if n.starts_with("Request declined") => Some(n.len()),
            _ => None,
        })
        .expect("the request is declined with a notice");
    assert!(declined < 400, "notice of {declined} bytes");
    match seen.last() {
        Some(Event::Finished {
            outcome: Outcome::Other(status),
        }) => assert!(status.len() <= 300, "status of {} bytes", status.len()),
        other => panic!("{other:?}"),
    }
    stop(&handle, task).await;
}

#[tokio::test]
async fn demo_cancel_in_the_dialog_is_interrupted() {
    let mut config = config();
    config.engine = Engine::DEMO;
    config.mode = Mode::Ask;
    let (handle, mut events, task) = spawn(config);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt("/approval-demo".into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await;
    handle.interrupt();
    let seen = until_finished(&mut events).await;
    assert!(
        matches!(
            seen.last(),
            Some(Event::Finished {
                outcome: Outcome::Interrupted
            })
        ),
        "{seen:?}"
    );
    assert!(!text_of(&seen).contains("Denied"), "{seen:?}");
    stop(&handle, task).await;
}

#[tokio::test]
async fn an_image_read_that_hangs_fails_the_turn() {
    /// Opens the FIFO for writing without blocking: a reader stuck in its
    /// open then returns.
    struct ReleaseFifo(std::path::PathBuf);
    impl Drop for ReleaseFifo {
        fn drop(&mut self) {
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.0);
        }
    }
    let dir = octet_testkit::TempDir::new("octet-live-fifo-image");
    std::fs::create_dir_all(dir.path()).unwrap();
    let path = dir.path().join("shot.png");
    std::fs::write(&path, b"png").unwrap();
    let image = ImageAttachment::open(&path).unwrap();
    // Swapped for a FIFO after it was attached: opening it waits for a writer.
    std::fs::remove_file(&path).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    // Releases the stuck open when the test ends, before the FIFO's
    // directory goes: otherwise the runtime would wait for it forever.
    let _release = ReleaseFifo(path.clone());
    let limits = Limits {
        image_read: Duration::from_millis(500),
        ..Limits::default()
    };
    let (handle, mut events, task) = spawn_with_limits(claude(), limits);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    let started = std::time::Instant::now();
    handle
        .send(Command::PromptWithDisplay {
            wire: "hello".into(),
            display: "hello".into(),
            images: vec![image],
        })
        .unwrap();
    let seen = until_finished(&mut events).await;
    assert!(
        matches!(
            seen.last(),
            Some(Event::Finished {
                outcome: Outcome::Failed
            })
        ),
        "{seen:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    stop(&handle, task).await;
}

#[tokio::test]
async fn launch_args_match_what_the_driver_runs() {
    let config = claude();
    let expected: Vec<String> = octet_engine::live::launch_args(&config)
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let (handle, mut events, task) = spawn(config);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt(scenario::ARGV.into())).unwrap();
    // The fake Claude echoes the arguments it was started with.
    assert_eq!(turn_text(&mut events).await, expected.join(" "));
    stop(&handle, task).await;
}

#[tokio::test]
async fn odd_claude_frames_are_ignored() {
    let init = r#"{"type":"control_response","response":{"subtype":"success","request_id":"octet-init","response":{}}}"#;
    let odd = [
        "[1,2]",
        r#""a string""#,
        "42",
        "null",
        r#"{"type":42}"#,
        r#"{"type":"assistant","message":"not an object"}"#,
        r#"{"type":"stream_event","event":{"delta":{"text":7}}}"#,
        r#"{"type":"result","session_id":5,"is_error":"no"}"#,
    ];
    let result = r#"{"type":"result","is_error":false,"result":"done","session_id":"s1"}"#;
    let mut body = format!("read line\necho '{init}'\nread line\n");
    for frame in odd {
        let _ = writeln!(body, "echo '{frame}'");
    }
    let _ = write!(body, "echo '{result}'\nexec sleep 30\n");
    let (_dir, vendor) = script_vendor("octet-live-odd-claude", &body);
    let (handle, mut events, task) =
        spawn(Config::new(Engine::CLAUDE, vendor, std::env::temp_dir()));
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let seen = until_finished(&mut events).await;
    assert!(
        matches!(seen.last(), Some(Event::Finished { .. })),
        "the session survived odd frames: {seen:?}"
    );
    stop(&handle, task).await;
}

#[tokio::test]
async fn odd_codex_frames_are_ignored() {
    let odd = [
        "[1,2]",
        "42",
        r#"{"method":7}"#,
        r#"{"method":"item/agentMessage/delta","params":"not an object"}"#,
        r#"{"method":"turn/completed","params":{"turn":"not an object"}}"#,
        r#"{"id":{"nested":true},"result":5}"#,
    ];
    let mut body = String::from(
        "read line\necho '{\"id\":1,\"result\":{}}'\nread line\nread line\n\
         echo '{\"id\":2,\"result\":{\"thread\":{\"id\":\"t1\"}}}'\n",
    );
    for frame in odd {
        let _ = writeln!(body, "echo '{frame}'");
    }
    body.push_str("exec sleep 30\n");
    let (_dir, vendor) = script_vendor("octet-live-odd-codex", &body);
    let (handle, mut events, task) =
        spawn(Config::new(Engine::CODEX, vendor, std::env::temp_dir()));
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    // The odd frames come after the session opened; it must still be up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        events
            .try_recv()
            .map_or(true, |e| !matches!(e, Event::Error(_) | Event::Stopped)),
        "the session survived odd frames"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn stopping_mid_turn_interrupts_the_turn_first() {
    // The fake Claude, with everything Octet writes to it logged.
    let child = octet_testkit::protocol_child();
    let (dir, script) = script_vendor(
        "octet-stop-mid-turn",
        &format!(
            "tee \"$(dirname \"$0\")/sent.log\" | '{}' \"$@\"",
            child.display()
        ),
    );
    let mut c = claude();
    c.binary = script;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    stop(&handle, task).await;
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    assert!(sent.contains(r#""subtype":"interrupt""#), "{sent}");
}

/// A vendor whose input is logged to `sent.log` beside it.
fn logged(name: &str) -> (octet_testkit::TempDir, std::path::PathBuf) {
    let child = octet_testkit::protocol_child();
    script_vendor(
        name,
        &format!(
            "tee \"$(dirname \"$0\")/sent.log\" | '{}' \"$@\"",
            child.display()
        ),
    )
}

#[tokio::test]
async fn a_codex_probe_lists_every_page_and_opens_no_thread() {
    let (dir, script) = logged("octet-probe-codex");
    let models = probe(
        Config::new(Engine::CODEX, script, std::env::temp_dir()),
        PROBE_LIMIT,
    )
    .await
    .unwrap()
    .models;
    let names: Vec<&str> = models.iter().map(|m| m.selection.as_str()).collect();
    assert_eq!(names, ["fixture", "other-full-id"]);
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    assert!(sent.contains("model/list"), "{sent}");
    assert!(!sent.contains("thread/"), "{sent}");
}

#[tokio::test]
async fn a_claude_probe_lists_its_models_and_sends_no_prompt() {
    let (dir, script) = logged("octet-probe-claude");
    let models = probe(
        Config::new(Engine::CLAUDE, script, std::env::temp_dir()),
        PROBE_LIMIT,
    )
    .await
    .unwrap()
    .models;
    assert_eq!(models[0].selection, "sonnet");
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    assert!(!sent.contains(r#""type":"user""#), "{sent}");
}

#[tokio::test]
async fn a_silent_cli_gives_up_at_the_limit() {
    let (_dir, script) = script_vendor("octet-probe-silent", "exec sleep 30");
    let started = std::time::Instant::now();
    let error = probe(
        Config::new(Engine::CODEX, script, std::env::temp_dir()),
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert!(error.contains("did not list its models"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_provider_without_a_list_is_not_probed() {
    let error = probe(
        Config::new(
            Engine::DEMO,
            std::path::PathBuf::from("demo"),
            std::env::temp_dir(),
        ),
        PROBE_LIMIT,
    )
    .await
    .unwrap_err();
    assert!(error.contains("cannot list its models"), "{error}");
}

/// A fake Codex app server in shell: logs what it is sent to `sent.log`,
/// answers `initialize`, each `model/list` with `listing`, and `skills/list`
/// with no skills.
fn scripted_codex(name: &str, listing: &str) -> (octet_testkit::TempDir, std::path::PathBuf) {
    script_vendor(
        name,
        &format!(
            r#"log="$(dirname "$0")/sent.log"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{{"id":%s,"result":{{}}}}\n' "$id" ;;
    *'"model/list"'*) printf '{{"id":%s,{listing}}}\n' "$id" ;;
    *'"skills/list"'*) printf '{{"id":%s,"result":{{"data":[]}}}}\n' "$id" ;;
  esac
done"#
        ),
    )
}

#[tokio::test]
async fn a_codex_probe_whose_listing_fails_opens_no_thread() {
    let (dir, script) = scripted_codex(
        "octet-probe-list-error",
        r#""error":{"code":-1,"message":"no models for you"}"#,
    );
    let error = probe(
        Config::new(Engine::CODEX, script, std::env::temp_dir()),
        PROBE_LIMIT,
    )
    .await
    .unwrap_err();
    assert!(error.contains("no models for you"), "{error}");
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    assert!(!sent.contains("thread/"), "{sent}");
}

#[tokio::test]
async fn a_codex_probe_that_pages_forever_ends_without_a_thread() {
    let (dir, script) = scripted_codex(
        "octet-probe-paging",
        r#""result":{"data":[{"id":"loop","model":"loop-model","displayName":"Loop"}],"nextCursor":"again"}"#,
    );
    let models = probe(
        Config::new(Engine::CODEX, script, std::env::temp_dir()),
        PROBE_LIMIT,
    )
    .await
    .unwrap()
    .models;
    assert_eq!(models.len(), 1);
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    assert_eq!(sent.matches("model/list").count(), 2, "{sent}");
    assert!(!sent.contains("thread/"), "{sent}");
}

#[tokio::test]
async fn a_silent_cli_is_gone_after_the_probe_gives_up() {
    let (dir, script) = script_vendor(
        "octet-probe-silent-pid",
        r#"echo $$ > "$(dirname "$0")/pid"; exec sleep 30"#,
    );
    // Long enough for the shell to start and write its PID on a busy machine.
    let error = probe(
        Config::new(Engine::CODEX, script, std::env::temp_dir()),
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(error.contains("did not list its models"), "{error}");
    let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the CLI {pid} is still running");
}

#[tokio::test]
async fn no_plugins_starts_codex_with_plugins_disabled() {
    let child = octet_testkit::protocol_child();
    let (dir, script) = script_vendor(
        "octet-codex-no-plugins",
        &format!(
            "echo \"$@\" > \"$(dirname \"$0\")/argv.log\"\nexec '{}' \"$@\"",
            child.display()
        ),
    );
    let mut c = config();
    c.binary = script;
    c.plugins.load = false;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    stop(&handle, task).await;
    let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
    assert_eq!(argv.trim(), "app-server --disable plugins");
}

#[tokio::test]
async fn both_providers_list_their_skills() {
    for (engine, name) in [
        (Engine::CLAUDE, "fixture-skill"),
        (Engine::CODEX, "fixture-codex-skill"),
    ] {
        let mut c = config();
        c.engine = engine;
        let (handle, mut events, task) = spawn(c);
        let skills = wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
        let Event::Skills(skills) = skills else {
            unreachable!()
        };
        assert_eq!(skills[0].name, name);
        stop(&handle, task).await;
    }
}

#[tokio::test]
async fn a_codex_skills_change_refreshes_the_list_mid_turn() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
    handle
        .send(Command::Prompt(scenario::SKILLS_CHANGED.into()))
        .unwrap();
    // The re-listed skills may arrive before or after the turn ends.
    let (mut skills, mut finished) = (false, false);
    while !(skills && finished) {
        match wait_for(&mut events, |e| {
            matches!(e, Event::Skills(_) | Event::Finished { .. })
        })
        .await
        {
            Event::Skills(_) => skills = true,
            _ => finished = true,
        }
    }
    stop(&handle, task).await;
}

#[tokio::test]
async fn a_probe_lists_models_and_skills() {
    let c = Config::new(
        Engine::CODEX,
        octet_testkit::protocol_child(),
        std::env::temp_dir(),
    );
    let listing = probe(c, PROBE_LIMIT).await.unwrap();
    assert!(!listing.models.is_empty());
    assert_eq!(listing.skills[0].name, "fixture-codex-skill");
}

#[tokio::test]
async fn a_claude_skill_is_sent_as_its_slash_command() {
    let (dir, script) = logged("octet-skill-claude");
    let mut c = claude();
    c.binary = script;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::Skill {
            name: "fixture-skill".into(),
            path: None,
            args: "go".into(),
            display: "/fixture-skill go".into(),
            context: None,
        })
        .unwrap();
    let user = wait_for(&mut events, |e| matches!(e, Event::User(_))).await;
    assert!(
        matches!(&user, Event::User(text) if text == "/fixture-skill go"),
        "{user:?}"
    );
    wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    stop(&handle, task).await;
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    let texts: Vec<String> = sent
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v["type"] == "user")
        .filter_map(|v| {
            let content = &v["message"]["content"];
            content
                .as_str()
                .map(str::to_owned)
                .or_else(|| content[0]["text"].as_str().map(str::to_owned))
        })
        .collect();
    assert_eq!(texts, ["/fixture-skill go"], "{sent}");
}

#[tokio::test]
async fn a_codex_skill_is_sent_as_a_skill_item_with_its_context() {
    let (dir, script) = logged("octet-skill-codex");
    let mut c = config();
    c.binary = script;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Skill {
            name: "fixture-codex-skill".into(),
            path: Some("/tmp/fixture-codex-skill/SKILL.md".into()),
            args: "go".into(),
            display: "/fixture-codex-skill go".into(),
            context: Some("EARLIER".into()),
        })
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    stop(&handle, task).await;
    let sent = std::fs::read_to_string(dir.path().join("sent.log")).unwrap();
    let start = sent
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["method"] == "turn/start")
        .expect("turn/start was sent");
    assert_eq!(
        start["params"]["input"],
        serde_json::json!([
            {"type":"skill","name":"fixture-codex-skill","path":"/tmp/fixture-codex-skill/SKILL.md"},
            {"type":"text","text":"EARLIER\n\ngo"}
        ])
    );
}

#[tokio::test]
async fn opencode_connects_lists_models_and_skills() {
    let (handle, mut events, task) = spawn(opencode());
    let (mut ready, mut models, mut skills, mut mode) = (None, None, None, None);
    while ready.is_none() || models.is_none() || skills.is_none() || mode.is_none() {
        match next(&mut events).await {
            Event::Ready { session } => ready = Some(session),
            Event::Models(list) => models = Some(list),
            Event::Skills(list) => skills = Some(list),
            Event::ModeChanged(changed) => mode = Some(changed),
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(ready.as_deref(), Some("fixture-ses"));
    let models = models.unwrap();
    let free = models
        .iter()
        .find(|m| m.selection == "opencode/fixture-free")
        .unwrap_or_else(|| panic!("{models:?}"));
    assert_eq!(free.name, "OpenCode/Fixture Free");
    assert_eq!(free.id.as_deref(), Some("opencode/fixture-free"));
    assert!(free.description.is_empty());
    let skills = skills.unwrap();
    assert!(
        skills.iter().any(|s| s.name == scenario::OPENCODE_SKILL),
        "{skills:?}"
    );
    assert_eq!(mode, Some(Mode::Ask));
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_turn_streams_text_and_usage() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    for _ in 0..2 {
        handle.send(Command::Prompt("hello".into())).unwrap();
        let (mut text, mut usage) = (String::new(), Vec::new());
        loop {
            match next(&mut events).await {
                Event::Text(t) => text.push_str(&t),
                Event::Usage(u) => usage.push(u),
                Event::Finished { outcome } => {
                    assert_eq!(outcome, Outcome::Completed);
                    break;
                }
                Event::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(text, scenario::OPENCODE_REPLY);
        assert_eq!(usage, ["42 tokens"]);
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_launches_acp_with_octet_agents() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["argv"][0], "acp", "{echo}");
    let content = echo["env"]["OPENCODE_CONFIG_CONTENT"].as_str().unwrap();
    assert!(content.contains("octet-ask"), "{content}");
    assert_eq!(echo["modes"], serde_json::json!(["octet-ask"]));
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_requested_model_is_set_and_confirmed() {
    let mut c = opencode();
    c.model = Some("openai/gpt-fixture".into());
    let (handle, mut events, task) = spawn(c);
    let selected = wait_for(&mut events, |e| matches!(e, Event::ModelSelected(_))).await;
    assert!(
        matches!(&selected, Event::ModelSelected(model) if model == "openai/gpt-fixture"),
        "{selected:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_resume_ignores_the_replayed_history() {
    let mut c = opencode();
    c.resume = Some("ses-old".into());
    let (handle, mut events, task) = spawn(c);
    let mut before = Vec::new();
    let session = loop {
        match next(&mut events).await {
            Event::Ready { session } => break session,
            Event::Error(error) => panic!("{error}"),
            event => before.push(event),
        }
    };
    assert_eq!(session, "ses-old");
    // The rest of the handshake, then the turn.
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    loop {
        match next(&mut events).await {
            Event::Started => break,
            Event::Error(error) => panic!("{error}"),
            event => before.push(event),
        }
    }
    for event in &before {
        assert!(
            !matches!(event, Event::Tool(_) | Event::Text(_)),
            "replayed: {event:?}"
        );
    }
    let text = turn_text(&mut events).await;
    assert_eq!(text, scenario::OPENCODE_REPLY);
    assert!(!text.contains("REPLAYED"));
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_fork_opens_a_new_session() {
    let mut c = opencode();
    c.resume = Some("ses-old".into());
    c.fork = true;
    let (handle, mut events, task) = spawn(c);
    let ready = wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    assert!(
        matches!(&ready, Event::Ready { session } if session == "fixture-fork"),
        "{ready:?}"
    );
    stop(&handle, task).await;
}
/// A 1×1 PNG.
const PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];
#[tokio::test]
async fn opencode_image_goes_inline() {
    let dir = octet_testkit::TempDir::new("octet-image-opencode");
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let (command, _) = image_prompt(&dir, PIXEL_PNG);
    handle.send(command).unwrap();
    let length = octet_engine::live::encoded_len(PIXEL_PNG.len() as u64);
    assert_eq!(
        turn_text(&mut events).await,
        format!("image image/png {length}")
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_handshake_out_of_order_is_an_error() {
    // opencode answers session/new (id 2) before initialize (id 1).
    let (_dir, vendor) = script_vendor(
        "octet-live-opencode-out-of-order",
        "read line\n\
         echo '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\",\"configOptions\":[]}}'\n\
         echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\n\
         exec sleep 30\n",
    );
    let (_handle, mut events, task) =
        spawn(Config::new(Engine::OPENCODE, vendor, std::env::temp_dir()));
    loop {
        match next(&mut events).await {
            Event::Error(error) => {
                assert_eq!(error, "Unexpected protocol initialization order");
                break;
            }
            Event::Ready { session } => panic!("a session opened: {session}"),
            _ => {}
        }
    }
    ended(task).await;
}
/// What an `ask` session stops with when opencode opens it with the mode
/// option `opened` and answers the handshake's agent switch with `reply`
/// (the reply's `result` or `error` member). The header must never have
/// claimed `ask` first.
async fn opencode_agent_refusal(opened: serde_json::Value, reply: serde_json::Value) -> String {
    use serde_json::json;
    let mut third = json!({"jsonrpc": "2.0", "id": 3});
    third
        .as_object_mut()
        .unwrap()
        .extend(reply.as_object().unwrap().clone());
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": [opened]}}),
        third,
    ];
    let mut body = String::new();
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    body.push_str("exec sleep 30\n");
    let (_dir, vendor) = script_vendor("octet-live-opencode-agent-refused", &body);
    let (_handle, mut events, task) =
        spawn(Config::new(Engine::OPENCODE, vendor, std::env::temp_dir()));
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::ModeChanged(Mode::Ask) => panic!("the header claimed ask"),
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    };
    ended(task).await;
    error
}
/// The error that stops a session whose agent opencode would not switch to.
fn agent_refused(reason: &str) -> String {
    format!(
        "opencode would not switch to agent octet-ask ({reason}); Octet stopped the session so opencode never acts outside the mode you chose"
    )
}
#[tokio::test]
async fn opencode_refused_agent_stops_the_session_though_build_is_kept() {
    let opened = serde_json::json!({"id": "mode", "currentValue": "build"});
    let refused = serde_json::json!({"error": {"code": -32602, "message": "unknown agent"}});
    assert_eq!(
        opencode_agent_refusal(opened, refused).await,
        agent_refused("unknown agent")
    );
}
#[tokio::test]
async fn opencode_refused_agent_stops_the_session_when_no_agent_was_reported() {
    let opened = serde_json::json!({"id": "model", "currentValue": "a/b"});
    let refused = serde_json::json!({"error": {"code": -32602, "message": "unknown agent"}});
    assert_eq!(
        opencode_agent_refusal(opened, refused).await,
        agent_refused("unknown agent")
    );
}
#[tokio::test]
async fn opencode_refused_agent_stops_the_session_when_an_unmapped_agent_is_kept() {
    let opened = serde_json::json!({"id": "mode", "currentValue": "plan"});
    let refused = serde_json::json!({"error": {"code": -32602, "message": "unknown agent"}});
    assert_eq!(
        opencode_agent_refusal(opened, refused).await,
        agent_refused("unknown agent")
    );
}
#[tokio::test]
async fn opencode_switch_reporting_another_agent_stops_the_session() {
    let opened = serde_json::json!({"id": "mode", "currentValue": "build"});
    let other = serde_json::json!({"result": {"configOptions": [opened.clone()]}});
    assert_eq!(
        opencode_agent_refusal(opened, other).await,
        agent_refused("opencode reported agent build instead of octet-ask")
    );
}
#[tokio::test]
async fn opencode_approval_allowed_sends_allow_once() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let Event::Approval {
        id,
        detail,
        summary,
    } = wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    assert!(detail.starts_with("execute · echo fixture"), "{detail}");
    assert!(detail.contains("\"command\": \"echo fixture\""), "{detail}");
    assert_eq!(summary.action, "Run a shell command");
    assert_eq!(summary.command.as_deref(), Some("echo fixture"));
    handle.send(Command::Answer { id, allow: true }).unwrap();
    let text = turn_text(&mut events).await;
    assert!(text.contains("once"), "{text}");
    assert!(!text.contains("always"), "{text}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_approval_denied_sends_reject() {
    let text = answer_first_approval(opencode(), scenario::APPROVAL, false).await;
    assert!(text.contains("reject"), "{text}");
}
#[tokio::test]
async fn opencode_approval_times_out_as_denied() {
    let mut c = opencode();
    c.approval_timeout = Duration::from_millis(200);
    let (handle, mut events, task) = spawn_with_limits(c, quick());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    wait_for(
        &mut events,
        |e| matches!(e, Event::ApprovalClosed(closed) if *closed == id),
    )
    .await;
    assert_eq!(turn_text(&mut events).await, "reject");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_nine_approvals_deny_the_ninth() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVALS_9.into()))
        .unwrap();
    let (mut shown, mut notices) = (Vec::new(), 0);
    loop {
        match next(&mut events).await {
            Event::Approval { id, .. } => shown.push(id),
            Event::Notice(text) if text.contains("Too many approvals are waiting") => {
                notices += 1;
            }
            // The ninth request was denied: opencode echoes the answer.
            Event::Text(text) if text == "answered 8:reject" => break,
            Event::Text(text) => panic!("unexpected answer: {text}"),
            Event::Finished { .. } | Event::Stopped => panic!("no ninth denial"),
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!((shown.len(), notices), (8, 1));
    // The eight shown still wait for the user.
    handle
        .send(Command::Answer {
            id: shown[0],
            allow: true,
        })
        .unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Text(t) if t == "answered 0:once"),
    )
    .await;
    handle.interrupt();
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_cancel_is_interrupted_though_it_says_end_turn() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.interrupt();
    let finished = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Interrupted
            }
        ),
        "{finished:?}"
    );
    // The next turn is not taken for a cancelled one.
    handle.send(Command::Prompt("hello".into())).unwrap();
    let finished = wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Completed
            }
        ),
        "{finished:?}"
    );
    stop(&handle, task).await;
}
/// The agents opencode has accepted so far, after a live switch to `target`.
async fn opencode_agents_after_switching_to(target: Mode) -> serde_json::Value {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(target)).unwrap();
    let changed = wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(_) | Event::Notice(_))
    })
    .await;
    assert!(
        matches!(&changed, Event::ModeChanged(mode) if *mode == target),
        "{changed:?}"
    );
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    stop(&handle, task).await;
    echo["modes"].clone()
}
#[tokio::test]
async fn opencode_live_switch_sets_the_agent() {
    let modes = opencode_agents_after_switching_to(Mode::AcceptEdits).await;
    assert_eq!(
        modes.as_array().unwrap().last().unwrap(),
        "octet-accept-edits",
        "{modes}"
    );
}
#[tokio::test]
async fn opencode_switch_during_a_turn_applies_from_the_next_turn() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::Prompt(scenario::HOLD.into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    // opencode applies an agent at the next prompt: the running turn keeps
    // octet-ask, and Octet says so as it does for Codex.
    let notice = next(&mut events).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t.starts_with("Mode applies from the next turn")),
        "{notice:?}"
    );
    handle.interrupt();
    wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(
        echo["agents"],
        serde_json::json!(["octet-ask", "octet-accept-edits"]),
        "{echo}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_switch_between_turns_gives_no_next_turn_notice() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let mut text = String::new();
    loop {
        match next(&mut events).await {
            Event::Notice(notice) => panic!("unexpected notice: {notice}"),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    let echo: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        echo["agents"],
        serde_json::json!(["octet-accept-edits"]),
        "{echo}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_auto_is_the_build_agent() {
    let modes = opencode_agents_after_switching_to(Mode::Auto).await;
    assert_eq!(
        modes.as_array().unwrap().last().unwrap(),
        "build",
        "{modes}"
    );
}
#[tokio::test]
async fn opencode_refused_switch_keeps_the_mode() {
    let mut c = opencode();
    c.model = Some(scenario::OPENCODE_REJECT_MODE.into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    let notice = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&notice, Event::Notice(text) if text.starts_with("opencode did not switch to accept-edits")),
        "{notice:?}"
    );
    let switched = timeout(
        Duration::from_secs(1),
        wait_for(&mut events, |e| {
            matches!(e, Event::ModeChanged(Mode::AcceptEdits))
        }),
    )
    .await;
    assert!(switched.is_err(), "{switched:?}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_full_access_launches_with_its_agent() {
    let mut c = opencode();
    c.mode = Mode::FullAccess;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::FullAccess))
    })
    .await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["modes"], serde_json::json!(["octet-full-access"]));
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_failed_turn_shows_the_message() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt(scenario::FAIL.into())).unwrap();
    let error = expect_error(&mut events).await;
    assert_eq!(error, "Internal error: fixture failure");
    let finished = next(&mut events).await;
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Failed
            }
        ),
        "{finished:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_other_session_updates_are_neither_progress_nor_output() {
    let (handle, mut events, task) = spawn_with_limits(opencode(), quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let sent = tokio::time::Instant::now();
    // Another session talks for about 2.4 seconds; ours is silent.
    handle
        .send(Command::Prompt(scenario::CHATTER.into()))
        .unwrap();
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::Text(text) => panic!("another session's text shown: {text}"),
            Event::Tool(tool) => panic!("another session's tool shown: {tool:?}"),
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    };
    assert!(error.contains("sent nothing for 1 second"), "{error}");
    // Had the chatter reset the 1 s limit, the error would come after 3.4 s.
    assert!(
        sent.elapsed() < Duration::from_secs(3),
        "{:?}",
        sent.elapsed()
    );
    let _ = handle;
    ended(task).await;
}
/// An `ask` session with a scripted opencode that, asked to switch to
/// accept-edits, answers `delay` seconds later with `reply` (the reply's
/// `result` or `error` member). The switch has been sent.
async fn opencode_switching(
    reply: serde_json::Value,
    delay: &str,
) -> (
    octet_testkit::TempDir,
    octet_engine::live::Handle,
    mpsc::Receiver<Event>,
    tokio::task::JoinHandle<()>,
) {
    use serde_json::json;
    let mode = |agent: &str| json!([{"id": "mode", "currentValue": agent}]);
    let mut fourth = json!({"jsonrpc": "2.0", "id": 4});
    fourth
        .as_object_mut()
        .unwrap()
        .extend(reply.as_object().unwrap().clone());
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": mode("build")}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {"configOptions": mode("octet-ask")}}),
    ];
    let mut body = String::new();
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    let _ = write!(
        body,
        "read line\nsleep {delay}\necho '{fourth}'\nexec sleep 30\n"
    );
    let (dir, vendor) = script_vendor("octet-live-opencode-switch", &body);
    let config = Config::new(Engine::OPENCODE, vendor, std::env::temp_dir());
    // A short confirmation window, and the usual connect limit: a shell
    // vendor can be slow to start while the other tests run.
    let limits = Limits {
        mode_confirm: Duration::from_secs(1),
        ..Limits::default()
    };
    let (handle, mut events, task) = spawn_with_limits(config, limits);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    (dir, handle, events, task)
}
/// What an `ask` session shows after asking a scripted opencode to switch
/// to accept-edits (see `opencode_switching`): its notices and header
/// modes, up to the `modes`th mode.
async fn opencode_scripted_switch(
    reply: serde_json::Value,
    delay: &str,
    modes: usize,
) -> Vec<String> {
    let (_dir, handle, mut events, task) = opencode_switching(reply, delay).await;
    let mut seen = Vec::new();
    while seen
        .iter()
        .filter(|e: &&String| e.starts_with("mode "))
        .count()
        < modes
    {
        match wait_for_within(&mut events, 5, |e| {
            matches!(e, Event::Notice(_) | Event::ModeChanged(_))
        })
        .await
        {
            Event::Notice(text) => seen.push(text),
            Event::ModeChanged(mode) => seen.push(format!("mode {}", mode.label())),
            _ => unreachable!(),
        }
    }
    stop(&handle, task).await;
    seen
}
/// The error that stops a session after a scripted switch (see
/// `opencode_switching`); the header must never have shown accept-edits.
async fn opencode_switch_error(reply: serde_json::Value, delay: &str) -> String {
    let (_dir, _handle, mut events, task) = opencode_switching(reply, delay).await;
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::ModeChanged(Mode::AcceptEdits) => panic!("the header claimed accept-edits"),
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    };
    ended(task).await;
    error
}
#[tokio::test]
async fn opencode_unconfirmed_switch_times_out_then_applies_a_late_confirmation() {
    let confirmed = serde_json::json!({"result": {"configOptions": [
        {"id": "mode", "currentValue": "octet-accept-edits"}]}});
    assert_eq!(
        opencode_scripted_switch(confirmed, "1.6", 2).await,
        [
            "opencode has not confirmed the switch to accept-edits",
            "mode ask",
            "opencode confirmed the switch to accept-edits late",
            "mode accept-edits",
        ]
    );
}
#[tokio::test]
async fn opencode_switch_reporting_another_agent_stops_or_keeps_the_mode() {
    // opencode ignored the switch: it still reports the agent in force.
    let ignored = serde_json::json!({"result": {"configOptions": [
        {"id": "mode", "currentValue": "octet-ask"}]}});
    assert_eq!(
        opencode_scripted_switch(ignored, "0", 1).await,
        [
            "opencode did not switch to accept-edits: opencode reported agent octet-ask",
            "mode ask",
        ]
    );
    // opencode moved to an agent nobody asked for: the session stops.
    for delay in ["0", "1.6"] {
        let other = serde_json::json!({"result": {"configOptions": [
            {"id": "mode", "currentValue": "build"}]}});
        assert_eq!(
            opencode_switch_error(other, delay).await,
            "opencode switched to agent build instead of octet-accept-edits; Octet stopped the session so opencode never acts outside the mode you chose",
            "after {delay} s"
        );
    }
}
#[tokio::test]
async fn opencode_permission_outside_a_turn_is_rejected_with_a_notice() {
    use serde_json::json;
    let dir = octet_testkit::TempDir::new("octet-live-opencode-stray-permission");
    std::fs::create_dir_all(dir.path()).unwrap();
    let reply_file = dir.path().join("reply");
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": []}}),
    ];
    let mut body = String::new();
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    // The handshake's agent switch, then a permission request while idle.
    let permission = json!({"jsonrpc": "2.0", "id": 0, "method": "session/request_permission",
        "params": {"sessionId": "s1", "toolCall": {"kind": "execute", "title": "rm -rf /"},
        "options": [{"optionId": "once", "kind": "allow_once"},
                    {"optionId": "reject", "kind": "reject_once"}]}});
    let _ = write!(
        body,
        "read line\necho '{}'\necho '{permission}'\nread reply\necho \"$reply\" > '{}'\nexec sleep 30\n",
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
        reply_file.display()
    );
    let vendor = octet_testkit::write_script(dir.path(), "vendor", &body);
    let (handle, mut events, task) =
        spawn(Config::new(Engine::OPENCODE, vendor, std::env::temp_dir()));
    let notice = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::Approval { .. })
    })
    .await;
    assert!(
        matches!(&notice, Event::Notice(text) if text == "opencode asked for a permission outside this session's running turn; Octet rejected it"),
        "{notice:?}"
    );
    // Octet replied before the notice; the vendor writes the reply down.
    timeout(Duration::from_secs(3), async {
        while !std::fs::read_to_string(&reply_file).is_ok_and(|reply| reply.ends_with("}\n")) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the vendor got no reply");
    let reply: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&reply_file).unwrap()).unwrap();
    assert_eq!(
        reply,
        json!({"jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "selected", "optionId": "reject"}}})
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_probe_lists_models_and_skills() {
    let listing = probe(opencode(), PROBE_LIMIT).await.unwrap();
    assert_eq!(listing.models.len(), 2, "{:?}", listing.models);
    assert!(
        listing
            .skills
            .iter()
            .any(|s| s.name == scenario::OPENCODE_SKILL),
        "{:?}",
        listing.skills
    );
}
#[tokio::test]
async fn opencode_probe_deletes_its_session() {
    // The fake's `session delete <id>` leaves `deleted-<id>` in its working
    // directory, which is the probe's.
    let dir = octet_testkit::TempDir::new("octet-opencode-probe-delete");
    std::fs::create_dir_all(dir.path()).unwrap();
    let mut c = opencode();
    c.cwd = dir.path().to_path_buf();
    probe(c, PROBE_LIMIT).await.unwrap();
    let marker = dir.path().join("deleted-fixture-ses");
    let started = tokio::time::Instant::now();
    while !marker.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the probe's session was not deleted"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
#[tokio::test]
async fn opencode_real_sessions_are_not_deleted() {
    let dir = octet_testkit::TempDir::new("octet-opencode-keep");
    std::fs::create_dir_all(dir.path()).unwrap();
    let mut c = opencode();
    c.cwd = dir.path().to_path_buf();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    stop(&handle, task).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!dir.path().join("deleted-fixture-ses").exists());
}
#[tokio::test]
async fn opencode_skill_runs_as_a_slash_prompt() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Skill {
            name: scenario::OPENCODE_SKILL.into(),
            path: None,
            args: "go".into(),
            display: format!("/{} go", scenario::OPENCODE_SKILL),
            context: None,
        })
        .unwrap();
    assert_eq!(
        turn_text(&mut events).await,
        format!("/{} go", scenario::OPENCODE_SKILL)
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn an_opencode_skill_with_context_still_starts_with_its_command() {
    // opencode runs a command only when the prompt starts with `/`; the
    // context goes in a block after it.
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Skill {
            name: scenario::OPENCODE_SKILL.into(),
            path: None,
            args: "go".into(),
            display: format!("/{} go", scenario::OPENCODE_SKILL),
            context: Some("earlier conversation".into()),
        })
        .unwrap();
    assert_eq!(
        turn_text(&mut events).await,
        format!("/{} go", scenario::OPENCODE_SKILL)
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_compact_uses_its_command() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Compact).unwrap();
    assert_eq!(turn_text(&mut events).await, "/compact");
    stop(&handle, task).await;
}
/// An opencode that opens its session and never lists any commands, then
/// answers the mode switch if one comes.
fn opencode_without_commands() -> (octet_testkit::TempDir, Config) {
    use serde_json::json;
    let options = json!([
        {"id": "model", "currentValue": "a/b", "options": [{"value": "a/b", "name": "A B"}]},
        {"id": "mode", "currentValue": "build", "options": [{"value": "build", "name": "build"}]},
    ]);
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": options}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
    ];
    let mut body = String::new();
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    body.push_str("exec sleep 30\n");
    let (dir, vendor) = script_vendor("octet-live-opencode-no-commands", &body);
    (
        dir,
        Config::new(Engine::OPENCODE, vendor, std::env::temp_dir()),
    )
}
#[tokio::test]
async fn opencode_probe_settles_for_the_models_when_no_commands_come() {
    let (_dir, config) = opencode_without_commands();
    let started = tokio::time::Instant::now();
    let listing = probe(config, PROBE_LIMIT).await.unwrap();
    assert_eq!(listing.models.len(), 1);
    assert!(listing.skills.is_empty());
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "{:?}",
        started.elapsed()
    );
}
#[tokio::test]
async fn opencode_compact_without_the_command_fails_the_turn() {
    let (_dir, config) = opencode_without_commands();
    let (handle, mut events, task) = spawn(config);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Compact).unwrap();
    let error = expect_error(&mut events).await;
    assert_eq!(error, "opencode does not offer compaction in this session");
    let finished = next(&mut events).await;
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Failed
            }
        ),
        "{finished:?}"
    );
    stop(&handle, task).await;
}
