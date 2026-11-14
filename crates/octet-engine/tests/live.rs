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
async fn a_claude_prompt_right_after_a_switch_is_not_held() {
    // Claude reads its input in order, so the switch reaches it before the
    // prompt: nothing waits for the (here 2.5 s late) confirmation.
    let mut c = claude();
    c.model = Some(scenario::LATE_MODE.into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::Auto)).unwrap();
    handle.send(Command::Prompt("hello".into())).unwrap();
    let first = wait_for(&mut events, |e| {
        matches!(e, Event::Started | Event::ModeChanged(_) | Event::Notice(_))
    })
    .await;
    assert!(matches!(first, Event::Started), "{first:?}");
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
/// The effort a Codex `PARAMS` turn asked for, after `/effort` set `effort`;
/// `Null` when the turn named none.
async fn codex_turn_effort(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    effort: Option<&str>,
) -> serde_json::Value {
    if let Some(effort) = effort {
        handle
            .send(Command::SetEffort(Some(effort.into())))
            .unwrap();
    } else {
        handle.send(Command::SetEffort(None)).unwrap();
    }
    wait_for(
        events,
        |e| matches!(e, Event::Notice(n) if n.starts_with("Reasoning effort")),
    )
    .await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(events).await).unwrap();
    echo["turn"]["effort"].clone()
}
/// A Codex session in `dir`, its catalog and config read.
async fn codex_settled(
    dir: &octet_testkit::TempDir,
) -> (
    octet_engine::live::Handle,
    mpsc::Receiver<Event>,
    tokio::task::JoinHandle<()>,
) {
    let mut c = config();
    c.cwd = dir.path().to_path_buf();
    let (handle, mut events, task) = spawn(c);
    // The second catalog page is asked for after the config.
    wait_for(
        &mut events,
        |e| matches!(e, Event::Models(m) if m.len() == 2),
    )
    .await;
    (handle, events, task)
}
#[tokio::test]
async fn codex_effort_default_goes_back_to_the_configured_level() {
    // Codex keeps a turn's effort for the turns after it, so "default" has
    // to name the level Codex would pick: here its config's.
    let dir = octet_testkit::TempDir::new("octet-live-codex-effort-config");
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("config-effort"), "high").unwrap();
    let (handle, mut events, task) = codex_settled(&dir).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    // A new thread already runs at the configured level.
    assert_eq!(echo["turn"]["effort"], serde_json::Value::Null, "{echo}");
    let low = codex_turn_effort(&handle, &mut events, Some("low")).await;
    assert_eq!(low, "low");
    let default = codex_turn_effort(&handle, &mut events, None).await;
    assert_eq!(default, "high");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_effort_default_without_a_configured_level_is_the_models() {
    let dir = octet_testkit::TempDir::new("octet-live-codex-effort-model");
    std::fs::create_dir_all(dir.path()).unwrap();
    let (handle, mut events, task) = codex_settled(&dir).await;
    let low = codex_turn_effort(&handle, &mut events, Some("low")).await;
    assert_eq!(low, "low");
    let default = codex_turn_effort(&handle, &mut events, None).await;
    assert_eq!(default, "medium");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_effort_default_on_a_resumed_thread_is_sent_whatever_the_reply_says() {
    // The resume reply may name the configured level rather than the one
    // the thread was left at: after a resume the thread's level is unknown.
    let dir = octet_testkit::TempDir::new("octet-live-codex-effort-resume");
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("config-effort"), "high").unwrap();
    let mut c = config();
    c.cwd = dir.path().to_path_buf();
    c.resume = Some("fixture-thread".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(
        &mut events,
        |e| matches!(e, Event::Models(m) if m.len() == 2),
    )
    .await;
    let default = codex_turn_effort(&handle, &mut events, None).await;
    assert_eq!(default, "high");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_resumed_prompt_sent_at_ready_goes_back_to_the_default() {
    // A headless run sends its prompt on ready, so a resumed thread is ready
    // only once Codex's config says what the default is.
    let dir = octet_testkit::TempDir::new("octet-live-codex-effort-ready");
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(dir.path().join("config-effort"), "high").unwrap();
    std::fs::write(dir.path().join("config-slow"), "").unwrap();
    let mut c = config();
    c.cwd = dir.path().to_path_buf();
    c.resume = Some("fixture-thread".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let echo: serde_json::Value = serde_json::from_str(&turn_text(&mut events).await).unwrap();
    assert_eq!(echo["turn"]["effort"], "high", "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_agent_messages_back_to_back_are_separate_paragraphs() {
    let (handle, mut events, task) = spawn(config());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::MESSAGES.into()))
        .unwrap();
    // Deltas of one message join; the next message starts a paragraph. A
    // tool call between messages already separates them.
    assert_eq!(turn_text(&mut events).await, "First.\n\nSecond.Third.");
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
    // Codex compacts without a word, so Octet says it is done.
    let done = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::Finished { .. })
    })
    .await;
    assert!(
        matches!(&done, Event::Notice(n) if n == "Context compacted"),
        "{done:?}"
    );
    let finished = next(&mut events).await;
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Completed
            }
        ),
        "{finished:?}"
    );
    // A later turn is not a compaction.
    handle.send(Command::Prompt("hello".into())).unwrap();
    let mut notices = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Notice(n) => notices.push(n),
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    assert!(
        !notices.iter().any(|n| n.contains("compacted")),
        "{notices:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn codex_compaction_that_fails_to_start_is_not_compacted() {
    let mut c = config();
    c.model = Some(scenario::REFUSE_COMPACT.into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Compact).unwrap();
    let mut notices = Vec::new();
    let mut errors = Vec::new();
    let outcome = loop {
        match next(&mut events).await {
            Event::Notice(n) => notices.push(n),
            Event::Error(e) => errors.push(e),
            Event::Finished { outcome } => break outcome,
            _ => {}
        }
    };
    assert_eq!(outcome, Outcome::Failed, "{errors:?}");
    assert!(
        errors.iter().any(|e| e.contains("compaction refused")),
        "{errors:?}"
    );
    assert!(
        !notices.iter().any(|n| n.contains("compacted")),
        "{notices:?}"
    );
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
async fn claude_named_by_a_start_hook_is_ready_once_and_resumable_after_its_first_turn() {
    let mut c = claude();
    c.model = Some(scenario::START_HOOK.into());
    let (handle, mut events, task) = spawn(c);
    // A SessionStart hook names the session before the handshake's reply,
    // but Claude saves the session only with its first turn: until then
    // there is none to resume, so the session is ready once, with no ID.
    let mut readies = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Ready { session } => readies.push(session),
            Event::ModeChanged(_) => break,
            _ => {}
        }
    }
    assert_eq!(readies, [""]);
    // The first turn names it again, and now it is kept.
    handle.send(Command::Prompt("hello".into())).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Ready { session } if session == "claude-fixture"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_named_by_a_slow_start_hook_after_its_reply_is_not_ready_with_that_id() {
    let mut c = claude();
    c.model = Some(scenario::SLOW_START_HOOK.into());
    let (handle, mut events, task) = spawn(c);
    // A slow SessionStart hook's frames come after the handshake's reply,
    // still before the first turn saves the session they name: a ready with
    // that ID would send a reconnect to a session Claude cannot resume.
    let mut readies = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Ready { session } => readies.push(session),
            Event::Hook { .. } => break,
            _ => {}
        }
    }
    assert_eq!(readies, [""]);
    // Only the first turn names the session.
    handle.send(Command::Prompt("hello".into())).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Ready { session } if session == "claude-fixture"),
    )
    .await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_takes_a_prompt_sent_at_ready_after_a_start_hook() {
    let mut c = claude();
    c.model = Some(scenario::START_HOOK.into());
    let (handle, mut events, task) = spawn(c);
    // Headless runs send their prompt on the first ready.
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let event = wait_for(&mut events, |e| {
        matches!(e, Event::Finished { .. } | Event::Notice(_))
    })
    .await;
    assert!(
        matches!(
            event,
            Event::Finished {
                outcome: Outcome::Completed
            }
        ),
        "{event:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn claude_local_command_keeps_the_session_model() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::SYNTHETIC.into()))
        .unwrap();
    let mut models = Vec::new();
    loop {
        match next(&mut events).await {
            Event::ModelSelected(model) => models.push(model),
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    // `<synthetic>` marks a reply no model wrote; it names no model.
    assert_eq!(models, ["claude-fixture-full-id"]);
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
async fn the_demo_names_every_provider_to_start_with() {
    let mut c = config();
    c.engine = Engine::DEMO;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let text = turn_text(&mut events).await;
    for engine in ["--engine codex", "--engine claude", "--engine opencode"] {
        assert!(text.contains(engine), "{text}");
    }
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
async fn an_opencode_model_refused_at_connect_stops_the_session() {
    // Running on opencode's own default instead would be a model, and
    // perhaps a bill, the user never chose.
    let mut c = opencode();
    c.model = Some(scenario::OPENCODE_UNKNOWN_MODEL.into());
    let (_handle, mut events, task) = spawn(c);
    loop {
        match next(&mut events).await {
            Event::Ready { session } => panic!("the session opened: {session}"),
            Event::Error(error) => {
                assert_eq!(
                    error,
                    format!(
                        "opencode refused the model {}: Invalid params: model not found",
                        scenario::OPENCODE_UNKNOWN_MODEL
                    )
                );
                break;
            }
            _ => {}
        }
    }
    ended(task).await;
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
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
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
async fn an_opencode_read_approval_names_the_file_its_tool_call_named() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::OPENCODE_READ_ENV.into()))
        .unwrap();
    let Event::Approval { id, summary, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(summary.action, "Read .env");
    assert_eq!(summary.files, ["/w/.env"]);
    handle.send(Command::Answer { id, allow: false }).unwrap();
    assert_eq!(turn_text(&mut events).await, "reject");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_approval_denied_sends_reject() {
    let text = answer_first_approval(opencode(), scenario::APPROVAL, false).await;
    assert!(text.contains("reject"), "{text}");
}
#[tokio::test]
async fn opencode_approval_times_out_as_denied() {
    // A second's window and the default limits: under load, neither a slow
    // start nor a slow reply may end the session before the timeout does.
    let mut c = opencode();
    c.approval_timeout = Duration::from_secs(1);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    let shown = tokio::time::Instant::now();
    // Unanswered, it is closed and denied by the timeout, which says so,
    // and opencode is told `reject`.
    let (mut seen, mut text) = (Vec::new(), String::new());
    loop {
        match wait_for_within(&mut events, 30, |_| true).await {
            Event::ApprovalClosed(done) if done == id => {
                assert!(
                    shown.elapsed() >= Duration::from_millis(900),
                    "{:?}",
                    shown.elapsed()
                );
                seen.push("closed".to_owned());
            }
            Event::Notice(notice) => seen.push(notice),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => break,
            _ => {}
        }
    }
    assert_eq!(seen, ["closed", "Approval timed out and was denied"]);
    assert_eq!(text, "reject");
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
    // opencode cannot steer: its `/steer` is the next prompt, under the new mode.
    assert!(
        matches!(&notice, Event::Notice(t) if !t.contains("steering")),
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
async fn opencode_prompt_sent_right_after_an_idle_switch_runs_under_the_new_agent() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    // Back to back, before opencode can confirm: the driver holds the
    // prompt until it does, so the header never shows ask over a turn
    // that runs under octet-accept-edits.
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let (mut seen, mut text) = (Vec::new(), String::new());
    loop {
        match next(&mut events).await {
            Event::Notice(notice) => seen.push(notice),
            Event::ModeChanged(mode) => seen.push(format!("mode {}", mode.label())),
            Event::Started => seen.push("started".into()),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    // Held, the prompt says what it waits for; no next-turn notice follows.
    assert_eq!(
        seen,
        [
            "Waiting for opencode to confirm the switch to accept-edits before sending your prompt",
            "mode accept-edits",
            "started"
        ]
    );
    let echo: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        echo["agents"],
        serde_json::json!(["octet-accept-edits"]),
        "{echo}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_compaction_held_for_a_switch_says_it_waits_to_start() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    handle.send(Command::Compact).unwrap();
    let notice = wait_for(&mut events, |e| matches!(e, Event::Notice(_))).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "Waiting for opencode to confirm the switch to accept-edits before starting the turn"),
        "{notice:?}"
    );
    assert_eq!(turn_text(&mut events).await, "/compact");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_turn_held_for_a_switch_never_starts_once_cancelled() {
    let confirmed = serde_json::json!({"result": {"configOptions": [
        {"id": "mode", "currentValue": "octet-accept-edits"}]}});
    let (dir, handle, mut events, task) =
        opencode_switching_then(confirmed, "0.8", PROMPT_LOGGED).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    // Time for the driver to take the prompt and hold it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.interrupt();
    let mut seen = Vec::new();
    while !seen.iter().any(|e: &String| e == "mode accept-edits") {
        match next(&mut events).await {
            Event::ModeChanged(mode) => seen.push(format!("mode {}", mode.label())),
            Event::Started => seen.push("started".into()),
            Event::Finished { outcome } => seen.push(format!("finished {outcome:?}")),
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        seen,
        ["started", "finished Interrupted", "mode accept-edits"]
    );
    // Nothing starts after the switch resolves, and opencode got no prompt.
    let started = timeout(
        Duration::from_millis(500),
        wait_for(&mut events, |e| matches!(e, Event::Started)),
    )
    .await;
    assert!(started.is_err(), "{started:?}");
    stop(&handle, task).await;
    assert!(!dir.path().join("prompt").exists());
}
/// What a held prompt's session shows until its turn ends, as text:
/// notices, header modes, turn starts, errors and the outcome.
async fn held_turn_events(events: &mut mpsc::Receiver<Event>) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        match wait_for_within(events, 8, |_| true).await {
            Event::Notice(text) => seen.push(text),
            Event::ModeChanged(mode) => seen.push(format!("mode {}", mode.label())),
            Event::Started => seen.push("started".into()),
            Event::Finished { outcome } => {
                seen.push(format!("finished {outcome:?}"));
                return seen;
            }
            _ => {}
        }
    }
}
#[tokio::test]
async fn opencode_turn_held_for_a_switch_that_times_out_is_not_sent() {
    let confirmed = serde_json::json!({"result": {"configOptions": [
        {"id": "mode", "currentValue": "octet-accept-edits"}]}});
    let (dir, handle, mut events, task) =
        opencode_switching_then(confirmed, "1.6", PROMPT_LOGGED).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let mut seen = Vec::new();
    let error = loop {
        match next(&mut events).await {
            Event::Notice(text) => seen.push(text),
            Event::ModeChanged(mode) => seen.push(format!("mode {}", mode.label())),
            Event::Started => seen.push("started".into()),
            Event::Error(error) => break error,
            _ => {}
        }
    };
    // opencode already has the switch, so the prompt could run under the
    // new agent while the header shows ask: it is not sent.
    assert_eq!(
        seen,
        [
            "Waiting for opencode to confirm the switch to accept-edits before sending your prompt",
            "opencode has not confirmed the switch to accept-edits",
            "mode ask",
            "started",
        ]
    );
    assert_eq!(
        error,
        "Not sent: opencode has not confirmed the switch to accept-edits; send it again once it does"
    );
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
    // The late confirmation, while idle, is a plain mode change.
    let mut late = Vec::new();
    while !late.iter().any(|e: &String| e.starts_with("mode ")) {
        match wait_for_within(&mut events, 5, |_| true).await {
            Event::Notice(text) => late.push(text),
            Event::ModeChanged(mode) => late.push(format!("mode {}", mode.label())),
            Event::Started => panic!("the held prompt started"),
            _ => {}
        }
    }
    assert_eq!(
        late,
        [
            "opencode confirmed the switch to accept-edits late",
            "mode accept-edits"
        ]
    );
    stop(&handle, task).await;
    assert!(!dir.path().join("prompt").exists());
}
#[tokio::test]
async fn opencode_turn_held_for_a_refused_switch_is_sent_under_the_old_mode() {
    let refused = serde_json::json!({"error": {"code": -32602, "message": "unknown agent"}});
    let (dir, handle, mut events, task) =
        opencode_switching_then(refused, "0.3", PROMPT_LOGGED).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    // The mode is known, unchanged: the prompt goes out.
    assert_eq!(
        held_turn_events(&mut events).await,
        [
            "Waiting for opencode to confirm the switch to accept-edits before sending your prompt",
            "opencode did not switch to accept-edits: unknown agent",
            "mode ask",
            "started",
            "finished Completed",
        ]
    );
    assert!(dir.path().join("prompt").exists());
    stop(&handle, task).await;
}
/// The events of one refused or finished turn, as text, up to its end.
async fn turn_events(events: &mut mpsc::Receiver<Event>) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        match next(events).await {
            Event::User(text) => seen.push(format!("user {text}")),
            Event::Started => seen.push("started".into()),
            Event::Error(error) => seen.push(format!("error {error}")),
            Event::Notice(text) => seen.push(text),
            Event::Finished { outcome } => {
                seen.push(format!("finished {outcome:?}"));
                return seen;
            }
            _ => {}
        }
    }
}
/// The refusal of a prompt sent while the switch to accept-edits is
/// unanswered.
const UNKNOWN_MODE: &str = "error Not sent: opencode has not confirmed the switch to accept-edits, \
     so its mode is unknown. Wait for it, or use /reconnect to start again in ask";
#[tokio::test]
async fn opencode_prompts_wait_for_a_timed_out_switch_then_run_under_its_agent() {
    // The gated fake answers the live switch only once the test releases it.
    let dir = octet_testkit::TempDir::new("octet-live-opencode-unknown-mode");
    std::fs::create_dir_all(dir.path()).unwrap();
    let mut c = opencode();
    c.cwd = dir.path().to_path_buf();
    c.model = Some(scenario::OPENCODE_GATED.into());
    let release = dir.path().join("release-mode");
    std::fs::write(&release, "").unwrap();
    let limits = Limits {
        mode_confirm: Duration::from_millis(300),
        ..Limits::default()
    };
    let (handle, mut events, task) = spawn_with_limits(c, limits);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    std::fs::remove_file(&release).unwrap();
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(t) if t.contains("has not confirmed")),
    )
    .await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    // opencode may already run the new agent: nothing is sent.
    handle.send(Command::Prompt("hello".into())).unwrap();
    assert_eq!(
        turn_events(&mut events).await,
        ["user hello", "started", UNKNOWN_MODE, "finished Failed"]
    );
    std::fs::write(&release, "").unwrap();
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    // Settled: prompts flow again, under the confirmed agent, and the
    // refused one never reached opencode.
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(
        echo["agents"],
        serde_json::json!(["octet-accept-edits"]),
        "{echo}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_prompts_run_under_the_old_agent_once_a_timed_out_switch_is_refused() {
    let refused = serde_json::json!({"error": {"code": -32602, "message": "unknown agent"}});
    let (dir, handle, mut events, task) =
        opencode_switching_then(refused, "1.6", PROMPT_LOGGED).await;
    wait_for_within(
        &mut events,
        5,
        |e| matches!(e, Event::Notice(t) if t.contains("has not confirmed")),
    )
    .await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    assert_eq!(
        turn_events(&mut events).await,
        ["user hello", "started", UNKNOWN_MODE, "finished Failed"]
    );
    // The late refusal settles it: the mode is still ask.
    wait_for_within(
        &mut events,
        5,
        |e| matches!(e, Event::Notice(t) if t.starts_with("opencode did not switch")),
    )
    .await;
    handle.send(Command::Prompt("again".into())).unwrap();
    assert_eq!(
        turn_events(&mut events).await,
        ["user again", "started", "finished Completed"]
    );
    let sent = std::fs::read_to_string(dir.path().join("prompt")).unwrap();
    assert!(sent.contains("again"), "{sent}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_reconnect_sets_the_agent_of_the_headers_mode() {
    // `/reconnect` resumes the session with its model, in the mode the
    // header shows; the handshake sets that mode's agent, so a mode left
    // unknown by an unanswered switch is known again.
    let mut c = opencode();
    c.resume = Some("ses-before".into());
    c.mode = Mode::AcceptEdits;
    c.model = Some("openai/gpt-fixture".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(echo["opened"], "session/resume", "{echo}");
    assert_eq!(echo["model"], "openai/gpt-fixture", "{echo}");
    assert_eq!(
        echo["modes"],
        serde_json::json!(["octet-accept-edits"]),
        "{echo}"
    );
    assert_eq!(
        echo["agents"],
        serde_json::json!(["octet-accept-edits"]),
        "{echo}"
    );
    stop(&handle, task).await;
}
/// The fake opencode's `PARAMS` echo, from a session `config` opens.
async fn opencode_params(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
) -> serde_json::Value {
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    serde_json::from_str(&turn_text(events).await).unwrap()
}
#[tokio::test]
async fn opencode_launch_effort_is_set_before_the_agent() {
    let mut c = opencode();
    c.effort = Some("high".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(echo["effort"], "high", "{echo}");
    assert_eq!(echo["efforts"], serde_json::json!(["high"]), "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_effort_changes_live() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetEffort(Some("max".into()))).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Effort(Some(l)) if l == "max"),
    )
    .await;
    let notice = next(&mut events).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "Reasoning effort: max (from now on)"),
        "{notice:?}"
    );
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(echo["effort"], "max", "{echo}");
    // `/effort default` goes back to the model's own level.
    handle.send(Command::SetEffort(None)).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Effort(None))).await;
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(echo["effort"], "low", "{echo}");
    stop(&handle, task).await;
}
/// Waits up to five seconds for `path` to exist.
async fn wait_for_file(path: &std::path::Path) {
    timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} did not appear", path.display()));
}
/// An opencode session on the gated model (see `scenario::OPENCODE_GATED`)
/// asking for effort `high` at launch, in a fresh directory.
fn opencode_gated(name: &str) -> (octet_testkit::TempDir, Config) {
    let dir = octet_testkit::TempDir::new(name);
    std::fs::create_dir_all(dir.path()).unwrap();
    let mut c = opencode();
    c.cwd = dir.path().to_path_buf();
    c.model = Some(scenario::OPENCODE_GATED.into());
    c.effort = Some("high".into());
    (dir, c)
}
/// The effort levels shown up to the session's first `ModeChanged`, which
/// ends the handshake, and then up to the next `Effort` event.
async fn efforts_shown(events: &mut mpsc::Receiver<Event>) -> Vec<Option<String>> {
    let mut shown = Vec::new();
    loop {
        match next(events).await {
            Event::Effort(level) => shown.push(level),
            Event::ModeChanged(_) => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    let Event::Effort(level) = wait_for(events, |e| matches!(e, Event::Effort(_))).await else {
        unreachable!()
    };
    shown.push(level);
    shown
}
#[tokio::test]
async fn opencode_effort_asked_while_the_handshake_sets_one_wins() {
    let (dir, c) = opencode_gated("octet-live-opencode-effort-in-flight");
    let (handle, mut events, task) = spawn(c);
    // The handshake's `high` is in flight when the user asks for `max`.
    wait_for_file(&dir.path().join("asked-effort")).await;
    handle.send(Command::SetEffort(Some("max".into()))).unwrap();
    std::fs::write(dir.path().join("release-effort"), "").unwrap();
    std::fs::write(dir.path().join("release-mode"), "").unwrap();
    // Its confirmation of `high` never shows over the newer `max`.
    assert_eq!(efforts_shown(&mut events).await, [Some("max".to_owned())]);
    let notice = next(&mut events).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "Reasoning effort: max (from now on)"),
        "{notice:?}"
    );
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(
        echo["efforts"],
        serde_json::json!(["high", "max"]),
        "{echo}"
    );
    assert_eq!(echo["effort"], "max", "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_effort_asked_after_the_handshake_set_one_is_still_sent() {
    let (dir, c) = opencode_gated("octet-live-opencode-effort-after-step");
    let (handle, mut events, task) = spawn(c);
    // The handshake has set `high` and waits on the agent.
    wait_for_file(&dir.path().join("asked-effort")).await;
    std::fs::write(dir.path().join("release-effort"), "").unwrap();
    wait_for_file(&dir.path().join("asked-mode")).await;
    handle.send(Command::SetEffort(Some("max".into()))).unwrap();
    std::fs::write(dir.path().join("release-mode"), "").unwrap();
    assert_eq!(
        efforts_shown(&mut events).await,
        [Some("high".to_owned()), Some("max".to_owned())]
    );
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(
        echo["efforts"],
        serde_json::json!(["high", "max"]),
        "{echo}"
    );
    assert_eq!(echo["effort"], "max", "{echo}");
    stop(&handle, task).await;
}
/// A scripted opencode that answers its requests with `frames` in turn,
/// writing every line it reads to `sent` in its directory.
fn opencode_logging(name: &str, frames: &[serde_json::Value]) -> (octet_testkit::TempDir, Config) {
    let mut body = String::from("log=\"$(dirname \"$0\")/sent\"\n");
    for frame in frames {
        let _ = write!(
            body,
            "read line\necho \"$line\" >> \"$log\"\necho '{frame}'\n"
        );
    }
    body.push_str("while read line; do echo \"$line\" >> \"$log\"; done\n");
    let (dir, vendor) = script_vendor(name, &body);
    (
        dir,
        Config::new(Engine::OPENCODE, vendor, std::env::temp_dir()),
    )
}
/// The requests a `opencode_logging` script has read, after a moment for
/// any still on their way.
async fn requests_sent(dir: &octet_testkit::TempDir) -> Vec<serde_json::Value> {
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::read_to_string(dir.path().join("sent"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[tokio::test]
async fn opencode_effort_default_after_a_bare_confirmation_goes_back() {
    use serde_json::json;
    let options = json!([{"id": "effort", "currentValue": "low",
        "options": [{"value": "low"}, {"value": "high"}]}]);
    // opencode confirms `high` without reporting its options again.
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": options}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 4, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 5, "result": {}}),
    ];
    let (dir, c) = opencode_logging("octet-live-opencode-bare-effort", &frames);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::SetEffort(Some("high".into())))
        .unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Effort(Some(l)) if l == "high"),
    )
    .await;
    handle.send(Command::SetEffort(None)).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Effort(None))).await;
    let sent = requests_sent(&dir).await;
    let values: Vec<&serde_json::Value> = sent
        .iter()
        .filter(|frame| frame["params"]["configId"] == "effort")
        .map(|frame| &frame["params"]["value"])
        .collect();
    assert_eq!(values, [&json!("high"), &json!("low")], "{sent:?}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_effort_default_with_unknown_levels_is_not_claimed() {
    use serde_json::json;
    // opencode never reports its options: the levels stay unknown.
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
        json!({"jsonrpc": "2.0", "id": 4, "result": {}}),
    ];
    let (dir, c) = opencode_logging("octet-live-opencode-unknown-effort", &frames);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::SetEffort(Some("high".into())))
        .unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Effort(Some(l)) if l == "high"),
    )
    .await;
    handle.send(Command::SetEffort(None)).unwrap();
    let notice = wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(t) if !t.contains("high (from")),
    )
    .await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "opencode keeps effort high: it has not reported its model's default level"),
        "{notice:?}"
    );
    // The interface goes back to the level in force.
    let effort = next(&mut events).await;
    assert!(
        matches!(&effort, Event::Effort(Some(l)) if l == "high"),
        "{effort:?}"
    );
    let sent = requests_sent(&dir).await;
    assert_eq!(sent.len(), 4, "{sent:?}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_effort_the_model_does_not_list_is_not_sent() {
    let mut c = opencode();
    c.effort = Some("high".into());
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::SetEffort(Some("bogus".into())))
        .unwrap();
    let notice = wait_for(&mut events, |e| matches!(e, Event::Notice(_))).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "opencode's model opencode/fixture-free takes effort low, high or max"),
        "{notice:?}"
    );
    // The interface goes back to the level opencode is at.
    let effort = next(&mut events).await;
    assert!(
        matches!(&effort, Event::Effort(Some(l)) if l == "high"),
        "{effort:?}"
    );
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(echo["efforts"], serde_json::json!(["high"]), "{echo}");
    assert_eq!(echo["effort"], "high", "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_model_without_effort_levels_says_so() {
    let mut c = opencode();
    c.model = Some("openai/gpt-fixture".into());
    c.effort = Some("high".into());
    let (handle, mut events, task) = spawn(c);
    // At connect: nothing is sent, and the level is dropped.
    let notice = wait_for(&mut events, |e| matches!(e, Event::Notice(_))).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "opencode's model openai/gpt-fixture has no effort levels"),
        "{notice:?}"
    );
    wait_for(&mut events, |e| matches!(e, Event::Effort(None))).await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::SetEffort(Some("high".into())))
        .unwrap();
    let notice = wait_for(&mut events, |e| matches!(e, Event::Notice(_))).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "opencode's model openai/gpt-fixture has no effort levels"),
        "{notice:?}"
    );
    let echo = opencode_params(&handle, &mut events).await;
    assert_eq!(echo["efforts"], serde_json::json!([]), "{echo}");
    assert_eq!(echo["effort"], serde_json::Value::Null, "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_effort_refused_at_connect_is_reported_and_the_session_goes_on() {
    use serde_json::json;
    // No options reported, so the level is sent; opencode refuses it.
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}}),
        json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32602, "message": "Invalid params: effort not found: high"}}),
        json!({"jsonrpc": "2.0", "id": 4, "result": {}}),
    ];
    let mut body = String::new();
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    body.push_str("exec sleep 30\n");
    let (_dir, vendor) = script_vendor("octet-live-opencode-effort-refused", &body);
    let mut c = Config::new(Engine::OPENCODE, vendor, std::env::temp_dir());
    c.effort = Some("high".into());
    let (handle, mut events, task) = spawn(c);
    let notice = wait_for(&mut events, |e| matches!(e, Event::Notice(_))).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "opencode did not set effort high: Invalid params: effort not found: high"),
        "{notice:?}"
    );
    wait_for(&mut events, |e| matches!(e, Event::Effort(None))).await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
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
async fn an_opencode_command_is_shown_by_its_command_not_its_folder() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::TOOL_FAIL.into()))
        .unwrap();
    let mut tools = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Tool(call) => tools.push((call.name, call.target, call.phase)),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    // The first frame names no command yet, only the working directory.
    assert_eq!(
        tools,
        [
            ("bash".to_owned(), String::new(), ToolPhase::Running),
            ("bash".to_owned(), "false".to_owned(), ToolPhase::Failed),
        ]
    );
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
/// What a scripted opencode (see `opencode_switching_then`) does after
/// its switch reply: write the next request, a prompt, to `prompt` in its
/// directory and end it.
const PROMPT_LOGGED: &str = "if read line; then echo \"$line\" > \"$(dirname \"$0\")/prompt\"; \
     echo '{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"stopReason\":\"end_turn\"}}'; fi\n\
     exec sleep 30\n";
/// A scripted session, the handle, its events and its task.
type Scripted = (
    octet_testkit::TempDir,
    octet_engine::live::Handle,
    mpsc::Receiver<Event>,
    tokio::task::JoinHandle<()>,
);
/// An `ask` session with a scripted opencode that, asked to switch to
/// accept-edits, answers `delay` seconds later with `reply` (the reply's
/// `result` or `error` member). The switch has been sent.
async fn opencode_switching(reply: serde_json::Value, delay: &str) -> Scripted {
    opencode_switching_then(reply, delay, "exec sleep 30\n").await
}
/// As `opencode_switching`, the script going on with `after` once it has
/// replied.
async fn opencode_switching_then(reply: serde_json::Value, delay: &str, after: &str) -> Scripted {
    use serde_json::json;
    let mode = |agent: &str| json!([{"id": "mode", "currentValue": agent}]);
    let mut fourth = json!({"jsonrpc": "2.0", "id": 4});
    fourth
        .as_object_mut()
        .unwrap()
        .extend(reply.as_object().unwrap().clone());
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": mode("build")}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {"configOptions": mode("octet-ask")}}),
    ];
    let mut body = String::new();
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    let _ = write!(body, "read line\nsleep {delay}\necho '{fourth}'\n{after}");
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
async fn opencode_refuses_a_switch_while_eight_wait_for_confirmation() {
    use serde_json::json;
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
    ];
    // opencode answers nothing after the handshake.
    let (dir, c) = opencode_logging("octet-live-opencode-late-switches", &frames);
    let limits = Limits {
        mode_confirm: Duration::from_millis(100),
        ..Limits::default()
    };
    let (handle, mut events, task) = spawn_with_limits(c, limits);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    for _ in 0..8 {
        handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
        wait_for(
            &mut events,
            |e| matches!(e, Event::Notice(t) if t.contains("has not confirmed")),
        )
        .await;
        wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    }
    // A late reply to a ninth could no longer be matched: it is not sent.
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    let notice = next(&mut events).await;
    assert!(
        matches!(&notice, Event::Notice(t) if t == "opencode has not confirmed 8 earlier mode switches; wait or reconnect"),
        "{notice:?}"
    );
    let mode = next(&mut events).await;
    assert!(matches!(mode, Event::ModeChanged(Mode::Ask)), "{mode:?}");
    let switches = requests_sent(&dir)
        .await
        .iter()
        .filter(|frame| frame["params"]["configId"] == "mode")
        .count();
    // The handshake's agent and the eight live switches.
    assert_eq!(switches, 9);
    stop(&handle, task).await;
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
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
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
async fn opencode_permission_for_another_session_during_a_turn_is_rejected() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::Prompt(scenario::OPENCODE_OTHER_PERMISSION.into()))
        .unwrap();
    let (mut notices, mut text) = (Vec::new(), String::new());
    loop {
        match next(&mut events).await {
            Event::Approval { detail, .. } => panic!("another session's request shown: {detail}"),
            Event::Notice(notice) => notices.push(notice),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        notices,
        ["opencode asked for a permission outside this session's running turn; Octet rejected it"]
    );
    // The fake echoes the answer it got.
    assert_eq!(text, "reject");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_file_writes_after_an_allowed_edit_are_refused_quietly() {
    // opencode 1.18.25 also asks the client to write an edit it was allowed,
    // although Octet offers no file system: refused, with nothing to show.
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::OPENCODE_EDIT.into()))
        .unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    handle.send(Command::Answer { id, allow: true }).unwrap();
    let (mut notices, mut text) = (Vec::new(), String::new());
    loop {
        match next(&mut events).await {
            Event::Notice(notice) => notices.push(notice),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    // The fake echoes the error code of Octet's reply to its write.
    assert_eq!(text, "oncefs -32601");
    assert!(notices.is_empty(), "{notices:?}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencode_permission_while_cancelling_is_rejected_and_says_so() {
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::Prompt(
            scenario::OPENCODE_PERMISSION_ON_CANCEL.into(),
        ))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.interrupt();
    let (mut notices, mut text) = (Vec::new(), String::new());
    let outcome = loop {
        match next(&mut events).await {
            Event::Approval { detail, .. } => panic!("shown while cancelling: {detail}"),
            Event::Notice(notice) => notices.push(notice),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { outcome } => break outcome,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(
        notices,
        ["opencode asked for a permission while its turn is being cancelled; Octet rejected it"]
    );
    assert_eq!(text, "reject");
    assert_eq!(outcome, Outcome::Interrupted);
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
/// An opencode skill `name` with `args`, as the interface sends it.
fn opencode_skill(name: &str, args: &str) -> Command {
    Command::Skill {
        name: name.into(),
        path: None,
        args: args.into(),
        display: format!("/{name} {args}"),
        context: None,
    }
}
/// The error and outcome that end the turn `command` starts in a session
/// in `mode`, and the text it streamed; it must end by itself.
async fn opencode_turn_ending(mode: Mode, command: Command) -> (Option<String>, Outcome, String) {
    let mut c = opencode();
    c.mode = mode;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(command).unwrap();
    let (mut error, mut text) = (None, String::new());
    let outcome = loop {
        match next(&mut events).await {
            Event::Error(message) => error = Some(message),
            Event::Text(t) => text.push_str(&t),
            Event::Approval { detail, .. } => panic!("an approval was shown: {detail}"),
            Event::Finished { outcome } => break outcome,
            _ => {}
        }
    };
    stop(&handle, task).await;
    (error, outcome, text)
}
#[tokio::test]
async fn opencodes_review_is_not_sent_while_the_mode_asks() {
    // `/review` runs as a subagent, whose approvals opencode never passes on:
    // in ask or accept-edits the turn would wait for ever.
    for (mode, command) in [
        (Mode::Ask, opencode_skill("review", "HEAD")),
        (Mode::AcceptEdits, opencode_skill("review", "")),
        (Mode::Ask, Command::Prompt("/review HEAD".into())),
    ] {
        let (error, outcome, text) = opencode_turn_ending(mode, command).await;
        assert_eq!(
            error.as_deref(),
            Some(subtask_refusal("/review", mode, "Octet did not send it").as_str())
        );
        assert_eq!(outcome, Outcome::Failed);
        // The fake echoes a command it is sent.
        assert_eq!(text, "");
    }
}
/// What Octet says when opencode's subagent command `command` cannot run
/// in `mode`, ending with what Octet did about it.
fn subtask_refusal(command: &str, mode: Mode, done: &str) -> String {
    format!(
        "opencode runs {command} in a subagent, whose approval requests it never passes on to Octet; \
         subagents are off in {}, so {done}. In auto, opencode's own permission settings decide",
        mode.label()
    )
}
#[tokio::test]
async fn opencodes_review_is_sent_in_auto() {
    let (error, outcome, text) =
        opencode_turn_ending(Mode::Auto, opencode_skill("review", "HEAD")).await;
    assert_eq!(error, None);
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(text, "/review HEAD");
}
#[tokio::test]
async fn an_opencode_subagent_started_while_the_mode_asks_stops_the_turn() {
    // A command opencode runs as a subagent although the definition Octet
    // read says otherwise (it changed since): its `task` call is the sign,
    // and the turn is cancelled.
    let (error, outcome, _) =
        opencode_turn_ending(Mode::Ask, opencode_skill(scenario::OPENCODE_SUBTASK, "")).await;
    let command = format!("/{}", scenario::OPENCODE_SUBTASK);
    assert_eq!(
        error.as_deref(),
        Some(subtask_refusal(&command, Mode::Ask, "Octet stopped the turn").as_str())
    );
    assert_eq!(outcome, Outcome::Failed);
}
#[tokio::test]
async fn an_opencode_subagent_in_auto_runs_until_cancelled() {
    let mut c = opencode();
    c.mode = Mode::Auto;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(opencode_skill(scenario::OPENCODE_SUBTASK, ""))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Tool(_))).await;
    // Nothing stops it: the subagent's approvals are opencode's to decide.
    let quiet = timeout(Duration::from_millis(500), async {
        loop {
            if let Event::Finished { .. } | Event::Error(_) = next(&mut events).await {
                break;
            }
        }
    })
    .await;
    assert!(quiet.is_err(), "the turn ended by itself");
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
    stop(&handle, task).await;
}
#[tokio::test]
async fn an_opencode_skill_never_carries_context() {
    // opencode joins a prompt's text blocks into a command's arguments and
    // runs every `!`cmd`` of the expanded template in its shell: earlier
    // conversation must never go with a command. The fake echoes every block.
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Skill {
            name: scenario::OPENCODE_SKILL.into(),
            path: None,
            args: "go".into(),
            display: format!("/{} go", scenario::OPENCODE_SKILL),
            context: Some("earlier !`touch pwned` conversation".into()),
        })
        .unwrap();
    let mut notices = Vec::new();
    let mut text = String::new();
    loop {
        match next(&mut events).await {
            Event::Notice(notice) => notices.push(notice),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Completed);
                break;
            }
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(text, format!("/{} go", scenario::OPENCODE_SKILL));
    assert!(
        notices
            .iter()
            .any(|n| n.contains("Octet did not send the earlier conversation with it")),
        "{notices:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn an_opencode_command_whose_arguments_run_shell_is_not_sent_while_the_mode_asks() {
    // opencode runs every `!`cmd`` of a command's expanded text in its shell
    // before any permission check.
    for (mode, command) in [
        (
            Mode::Ask,
            opencode_skill(scenario::OPENCODE_SKILL, "a !`touch pwned` b"),
        ),
        (
            Mode::AcceptEdits,
            opencode_skill(scenario::OPENCODE_SKILL, "!`id`"),
        ),
        (
            Mode::Ask,
            Command::Prompt(format!("/{} !`touch pwned`", scenario::OPENCODE_SKILL)),
        ),
    ] {
        let (error, outcome, text) = opencode_turn_ending(mode, command).await;
        let error = error.expect("the turn was refused");
        assert!(
            error.contains("which opencode runs in its shell before it asks for any permission"),
            "{error}"
        );
        assert!(error.contains(mode.label()), "{error}");
        assert_eq!(outcome, Outcome::Failed);
        assert_eq!(text, "", "it was sent");
    }
    // In auto, opencode's own settings decide.
    let (error, outcome, text) = opencode_turn_ending(
        Mode::Auto,
        opencode_skill(scenario::OPENCODE_SKILL, "!`true`"),
    )
    .await;
    assert_eq!(error, None);
    assert_eq!(outcome, Outcome::Completed);
    assert_eq!(text, format!("/{} !`true`", scenario::OPENCODE_SKILL));
}
#[tokio::test]
async fn opencode_compact_is_sent_though_opencode_does_not_list_it() {
    // opencode 1.18.25 summarizes the session for a `/compact` prompt
    // although its command list never names `compact`.
    let (handle, mut events, task) = spawn(opencode());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Compact).unwrap();
    assert_eq!(turn_text(&mut events).await, "/compact");
    stop(&handle, task).await;
}
/// An opencode that opens its session and never lists any commands, then
/// answers the mode switch if one comes. Its `session delete ID` leaves
/// `deleted-ID` in its working directory.
fn opencode_without_commands() -> (octet_testkit::TempDir, Config) {
    use serde_json::json;
    let options = json!([
        {"id": "model", "currentValue": "a/b", "options": [{"value": "a/b", "name": "A B"}]},
        {"id": "mode", "currentValue": "build", "options": [{"value": "build", "name": "build"}]},
    ]);
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1", "configOptions": options}}),
        json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
    ];
    let mut body = String::from("if [ \"$1\" = session ]; then touch \"deleted-$3\"; exit 0; fi\n");
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    body.push_str("exec sleep 30\n");
    let (dir, vendor) = script_vendor("octet-live-opencode-no-commands", &body);
    let config = Config::new(Engine::OPENCODE, vendor, dir.path());
    (dir, config)
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
/// Waits up to five seconds for the probe vendor in `dir` to have been
/// asked to delete session `s1`.
async fn probe_session_deleted(dir: &octet_testkit::TempDir) {
    let marker = dir.path().join("deleted-s1");
    timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the probe's session was not deleted");
}
#[tokio::test]
async fn opencode_probe_that_gives_up_still_deletes_its_session() {
    let (dir, config) = opencode_without_commands();
    // Run the new script once first: its first start can take seconds
    // while macOS scans it, and the probe must have opened its session
    // before it gives up.
    let warm = std::process::Command::new(&config.binary)
        .args(["session", "delete", "warm"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(warm.success());
    // The probe gives up shortly before its two-second wait for the
    // commands would end; the session opened well before that.
    let error = probe(config, Duration::from_millis(1900))
        .await
        .unwrap_err();
    assert_eq!(error, "opencode did not list its models within 1 s");
    probe_session_deleted(&dir).await;
}
#[tokio::test]
async fn opencode_probe_that_fails_still_deletes_its_session() {
    use serde_json::json;
    // opencode opens the session, then exits before listing anything.
    let frames = [
        json!({"jsonrpc": "2.0", "id": 1, "result": {"agentInfo": {"version": "1.18.25"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "s1"}}),
    ];
    let mut body = String::from("if [ \"$1\" = session ]; then touch \"deleted-$3\"; exit 0; fi\n");
    for frame in &frames {
        let _ = write!(body, "read line\necho '{frame}'\n");
    }
    let (dir, vendor) = script_vendor("octet-live-opencode-probe-fails", &body);
    let config = Config::new(Engine::OPENCODE, vendor, dir.path());
    let error = probe(config, PROBE_LIMIT).await.unwrap_err();
    assert!(error.starts_with("Vendor disconnected"), "{error}");
    probe_session_deleted(&dir).await;
}
/// A workspace for the fake opencode, with `commands` as the extra command
/// definitions its `debug config` shows (`fixture-commands.json`) and
/// `skills` as what its `debug skill` shows, and an empty `debug-calls`.
fn opencode_workspace(
    name: &str,
    commands: &serde_json::Value,
    skills: &serde_json::Value,
) -> octet_testkit::TempDir {
    let dir = octet_testkit::TempDir::new(name);
    std::fs::create_dir_all(dir.path()).unwrap();
    std::fs::write(
        dir.path().join("fixture-commands.json"),
        commands.to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("fixture-skills.json"), skills.to_string()).unwrap();
    std::fs::write(dir.path().join("debug-calls"), "").unwrap();
    dir
}
/// The error, outcome and text of the turn `command` starts in a session in
/// `mode` working in `dir`, with `limits`; it must end by itself.
async fn opencode_turn_in(
    dir: &octet_testkit::TempDir,
    mode: Mode,
    limits: Limits,
    command: Command,
) -> (Option<String>, Outcome, String) {
    let mut c = opencode();
    c.mode = mode;
    c.cwd = dir.path().to_path_buf();
    let (handle, mut events, task) = spawn_with_limits(c, limits);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle.send(command).unwrap();
    let (mut error, mut text) = (None, String::new());
    let outcome = loop {
        match next(&mut events).await {
            Event::Error(message) => error = Some(message),
            Event::Text(t) => text.push_str(&t),
            Event::Approval { detail, .. } => panic!("an approval was shown: {detail}"),
            Event::Finished { outcome } => break outcome,
            _ => {}
        }
    };
    stop(&handle, task).await;
    (error, outcome, text)
}
/// Asserts that `command`, sent in `mode` in `dir`, ended Failed unsent
/// with an error that contains `why` and names the mode.
async fn assert_opencode_refuses(
    dir: &octet_testkit::TempDir,
    mode: Mode,
    command: Command,
    why: &str,
) {
    let (error, outcome, text) = opencode_turn_in(dir, mode, Limits::default(), command).await;
    let error = error.expect("the turn was refused");
    assert!(error.contains(why), "{error}");
    assert!(error.contains(mode.label()), "{error}");
    assert_eq!(outcome, Outcome::Failed);
    assert_eq!(text, "", "it was sent");
}
#[tokio::test]
async fn an_opencode_command_under_another_agent_is_not_sent_while_the_mode_asks() {
    // A command's own `agent` replaces the mode's: under `build` its edits
    // and commands would follow the user's own permissions, unasked.
    let dir = opencode_workspace(
        "octet-opencode-agent",
        &serde_json::json!({
            "fixture-build": {"agent": "build", "template": "Edit it"},
            "fixture-general": {"agent": "general", "template": "Edit it"},
            "fixture-own": {"agent": "octet-ask", "template": "Say hi"},
        }),
        &serde_json::json!([]),
    );
    for (mode, name) in [
        (Mode::Ask, "fixture-build"),
        (Mode::AcceptEdits, "fixture-build"),
        (Mode::Ask, "fixture-general"),
        (Mode::AcceptEdits, "fixture-own"),
    ] {
        let why = if name == "fixture-general" {
            "agent general"
        } else if name == "fixture-own" {
            "agent octet-ask"
        } else {
            "agent build"
        };
        assert_opencode_refuses(&dir, mode, opencode_skill(name, ""), why).await;
    }
    // The mode's own agent changes nothing.
    let (error, outcome, text) = opencode_turn_in(
        &dir,
        Mode::Ask,
        Limits::default(),
        opencode_skill("fixture-own", ""),
    )
    .await;
    assert_eq!((error, outcome), (None, Outcome::Completed));
    assert_eq!(text, "/fixture-own");
    // In auto, opencode's own settings decide.
    let (error, outcome, text) = opencode_turn_in(
        &dir,
        Mode::Auto,
        Limits::default(),
        opencode_skill("fixture-build", ""),
    )
    .await;
    assert_eq!((error, outcome), (None, Outcome::Completed));
    assert_eq!(text, "/fixture-build");
}
#[tokio::test]
async fn an_opencode_subtask_command_is_not_sent_while_the_mode_asks() {
    let dir = opencode_workspace(
        "octet-opencode-subtask",
        &serde_json::json!({"fixture-sub": {"subtask": true, "template": "Edit it"}}),
        &serde_json::json!([]),
    );
    assert_opencode_refuses(
        &dir,
        Mode::Ask,
        Command::Prompt("/fixture-sub now".into()),
        &subtask_refusal("/fixture-sub", Mode::Ask, "Octet did not send it"),
    )
    .await;
}
#[tokio::test]
async fn an_opencode_command_whose_text_runs_shell_is_not_sent_while_the_mode_asks() {
    // opencode runs each `!`cmd`` of a command's expanded template in its
    // shell before any permission check: a template's own, one its
    // arguments complete, and a skill's.
    let dir = opencode_workspace(
        "octet-opencode-template-shell",
        &serde_json::json!({
            "fixture-date": {"template": "Today: !`date`"},
            "fixture-bang": {"template": "Run !$1 now"},
            "fixture-plain": {"template": "Say $ARGUMENTS"},
        }),
        &serde_json::json!([
            {"name": "fixture-shell-skill", "description": "", "location": "/s/SKILL.md", "content": "x !`id` y"}
        ]),
    );
    assert_opencode_refuses(
        &dir,
        Mode::Ask,
        opencode_skill("fixture-date", ""),
        "its template runs",
    )
    .await;
    assert_opencode_refuses(
        &dir,
        Mode::Ask,
        opencode_skill("fixture-shell-skill", ""),
        "its template runs",
    )
    .await;
    assert_opencode_refuses(
        &dir,
        Mode::AcceptEdits,
        opencode_skill("fixture-bang", "`touch pwned`"),
        "which opencode runs in its shell before it asks for any permission",
    )
    .await;
    let (error, outcome, text) = opencode_turn_in(
        &dir,
        Mode::Ask,
        Limits::default(),
        opencode_skill("fixture-plain", "hi `x`"),
    )
    .await;
    assert_eq!((error, outcome), (None, Outcome::Completed));
    assert_eq!(text, "/fixture-plain hi `x`");
}
#[tokio::test]
async fn an_opencode_command_octet_cannot_check_is_not_sent_while_the_mode_asks() {
    // Fail closed: what Octet cannot read, it does not send.
    let dir = opencode_workspace(
        "octet-opencode-unknown",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    assert_opencode_refuses(
        &dir,
        Mode::Ask,
        Command::Prompt("/nowhere x".into()),
        "no definition",
    )
    .await;
    // A listed command whose definition differs from the list (an MCP
    // prompt of the same name takes the command's place in opencode).
    std::fs::write(
        dir.path().join("fixture-commands.json"),
        serde_json::json!({scenario::OPENCODE_SKILL: {"description": "Another", "template": "x"}})
            .to_string(),
    )
    .unwrap();
    assert_opencode_refuses(
        &dir,
        Mode::Ask,
        opencode_skill(scenario::OPENCODE_SKILL, ""),
        "another description",
    )
    .await;
    std::fs::write(dir.path().join("fixture-commands.json"), "{}").unwrap();
    std::fs::write(dir.path().join("debug-fails"), "").unwrap();
    assert_opencode_refuses(
        &dir,
        Mode::Ask,
        opencode_skill(scenario::OPENCODE_SKILL, ""),
        "could not read",
    )
    .await;
    std::fs::remove_file(dir.path().join("debug-fails")).unwrap();
    std::fs::write(dir.path().join("debug-hangs"), "").unwrap();
    let limits = Limits {
        command_check: Duration::from_millis(500),
        ..Limits::default()
    };
    let started = tokio::time::Instant::now();
    let (error, outcome, text) = opencode_turn_in(
        &dir,
        Mode::Ask,
        limits,
        opencode_skill(scenario::OPENCODE_SKILL, ""),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        error.as_deref().unwrap_or("").contains("could not read"),
        "{error:?}"
    );
    assert_eq!((outcome, text.as_str()), (Outcome::Failed, ""));
}
#[tokio::test]
async fn opencodes_commands_are_read_once_and_again_when_the_list_changes() {
    let dir = opencode_workspace(
        "octet-opencode-relist",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    let mut c = opencode();
    c.cwd = dir.path().to_path_buf();
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    for _ in 0..2 {
        handle
            .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
            .unwrap();
        assert_eq!(
            turn_text(&mut events).await,
            format!("/{} go", scenario::OPENCODE_SKILL)
        );
    }
    let calls = || std::fs::read_to_string(dir.path().join("debug-calls")).unwrap();
    assert_eq!(calls().matches("config").count(), 1, "{}", calls());
    std::fs::write(
        dir.path().join("fixture-commands.json"),
        serde_json::json!({scenario::OPENCODE_LATE: {"description": "A late fixture command", "template": "x"}})
            .to_string(),
    )
    .unwrap();
    handle
        .send(Command::Prompt(scenario::OPENCODE_RELIST.into()))
        .unwrap();
    turn_text(&mut events).await;
    handle
        .send(opencode_skill(scenario::OPENCODE_LATE, ""))
        .unwrap();
    assert_eq!(
        turn_text(&mut events).await,
        format!("/{}", scenario::OPENCODE_LATE)
    );
    assert_eq!(calls().matches("config").count(), 2, "{}", calls());
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_quiet_opencode_subagent_gets_a_notice_where_opencode_decides() {
    // In auto and full access, a subagent whose own settings ask waits for
    // a permission opencode never passes on: once the turn goes quiet,
    // Octet says so, without claiming it is stuck.
    for mode in [Mode::Auto, Mode::FullAccess] {
        let mut c = opencode();
        c.mode = mode;
        let limits = Limits {
            subagent_quiet: Duration::from_millis(300),
            ..Limits::default()
        };
        let (handle, mut events, task) = spawn_with_limits(c, limits);
        wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
        handle
            .send(opencode_skill(scenario::OPENCODE_SUBTASK, ""))
            .unwrap();
        let notice = timeout(
            Duration::from_secs(3),
            wait_for(&mut events, |e| {
                matches!(e, Event::Notice(_) | Event::Finished { .. })
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("{mode:?}: no notice"));
        match notice {
            Event::Notice(notice) => assert_eq!(
                notice,
                "opencode's subagent has gone quiet: it may still be working, or waiting for a \
                 permission Octet cannot show; Esc cancels",
                "{mode:?}"
            ),
            other => panic!("{mode:?}: {other:?}"),
        }
        // Once is enough.
        let again = timeout(Duration::from_millis(800), async {
            loop {
                if let Event::Notice(_) | Event::Finished { .. } = next(&mut events).await {
                    break;
                }
            }
        })
        .await;
        assert!(again.is_err(), "another notice, or the turn ended");
        handle.interrupt();
        wait_for(&mut events, |e| matches!(e, Event::Finished { .. })).await;
        stop(&handle, task).await;
    }
}
/// A session in `mode` working in `dir`, ready.
async fn opencode_session_in(
    dir: &octet_testkit::TempDir,
    mode: Mode,
    limits: Limits,
) -> (
    octet_engine::live::Handle,
    mpsc::Receiver<Event>,
    tokio::task::JoinHandle<()>,
) {
    let mut c = opencode();
    c.mode = mode;
    c.cwd = dir.path().to_path_buf();
    let (handle, mut events, task) = spawn_with_limits(c, limits);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    (handle, events, task)
}
/// What Octet says while a command waits for its check.
fn checking(name: &str) -> String {
    format!("Checking /{name} against opencode's definitions…")
}
#[tokio::test]
async fn esc_while_an_opencode_command_waits_for_its_check_cancels_at_once() {
    // The reading never ends; the session must not wait for it.
    let dir = opencode_workspace(
        "octet-opencode-check-esc",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-hangs"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    handle
        .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
        .unwrap();
    let notice = checking(scenario::OPENCODE_SKILL);
    timeout(
        Duration::from_secs(5),
        wait_for(
            &mut events,
            |e| matches!(e, Event::Notice(n) if *n == notice),
        ),
    )
    .await
    .expect("no notice that the command waits for its check");
    let cancelled = tokio::time::Instant::now();
    handle.interrupt();
    let finished = timeout(
        Duration::from_secs(25),
        wait_for(&mut events, |e| matches!(e, Event::Finished { .. })),
    )
    .await
    .expect("the turn never ended");
    let took = cancelled.elapsed();
    assert!(
        matches!(
            finished,
            Event::Finished {
                outcome: Outcome::Interrupted
            }
        ),
        "{finished:?} after {took:?}"
    );
    assert!(took < Duration::from_millis(500), "Esc took {took:?}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn quitting_while_an_opencode_command_waits_for_its_check_stops_at_once() {
    let dir = opencode_workspace(
        "octet-opencode-check-quit",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-hangs"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    handle
        .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
        .unwrap();
    let notice = checking(scenario::OPENCODE_SKILL);
    wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(n) if *n == notice),
    )
    .await;
    let quit = tokio::time::Instant::now();
    stop(&handle, task).await;
    assert!(
        quit.elapsed() < Duration::from_secs(2),
        "{:?}",
        quit.elapsed()
    );
}
#[tokio::test]
async fn opencodes_frames_keep_coming_while_a_command_waits_for_its_check() {
    let dir = opencode_workspace(
        "octet-opencode-check-frames",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-hangs"), "").unwrap();
    std::fs::write(dir.path().join("ticks"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    handle
        .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
        .unwrap();
    let notice = checking(scenario::OPENCODE_SKILL);
    timeout(
        Duration::from_secs(5),
        wait_for(
            &mut events,
            |e| matches!(e, Event::Notice(n) if *n == notice),
        ),
    )
    .await
    .expect("no notice that the command waits for its check");
    // The fake lists its commands every 100 ms.
    for _ in 0..3 {
        timeout(
            Duration::from_secs(1),
            wait_for(&mut events, |e| matches!(e, Event::Skills(_))),
        )
        .await
        .expect("opencode's frames stopped while the command waited");
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn opencodes_commands_are_read_when_it_lists_them_in_every_mode() {
    for mode in [Mode::Auto, Mode::FullAccess] {
        let dir = opencode_workspace(
            "octet-opencode-read-at-connect",
            &serde_json::json!({}),
            &serde_json::json!([]),
        );
        let (handle, _events, task) = opencode_session_in(&dir, mode, Limits::default()).await;
        let calls = || std::fs::read_to_string(dir.path().join("debug-calls")).unwrap();
        timeout(Duration::from_secs(5), async {
            while !calls().contains("config") {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{mode:?}: no reading: {:?}", calls()));
        stop(&handle, task).await;
    }
}
#[tokio::test]
async fn a_reading_that_failed_in_the_background_is_tried_again_once() {
    let dir = opencode_workspace(
        "octet-opencode-check-retry",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-fails"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    let calls = || std::fs::read_to_string(dir.path().join("debug-calls")).unwrap();
    let configs = || calls().matches("config").count();
    timeout(Duration::from_secs(5), async {
        while configs() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no reading at connect");
    // The background reading failed; one that works now is used.
    std::fs::remove_file(dir.path().join("debug-fails")).unwrap();
    handle
        .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
        .unwrap();
    assert_eq!(
        turn_text(&mut events).await,
        format!("/{} go", scenario::OPENCODE_SKILL)
    );
    assert_eq!(configs(), 2, "{}", calls());
    stop(&handle, task).await;
}
#[tokio::test]
async fn compact_does_not_wait_for_a_reading_when_no_command_is_named_compact() {
    let dir = opencode_workspace(
        "octet-opencode-check-compact",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-hangs"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    handle.send(Command::Compact).unwrap();
    assert_eq!(turn_text(&mut events).await, "/compact");
    stop(&handle, task).await;
}
/// The fixture skill's definition, as `debug config` shows it, running
/// `template`.
fn fixture_skill_running(template: &str) -> serde_json::Value {
    serde_json::json!({scenario::OPENCODE_SKILL: {
        "description": "An opencode fixture skill", "template": template}})
}
/// Switches the session to `mode` and waits for the switch.
async fn switch_to(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    mode: Mode,
) {
    handle.send(Command::SetMode(mode)).unwrap();
    wait_for(events, |e| matches!(e, Event::ModeChanged(m) if *m == mode)).await;
}
/// The error and outcome of the turn `command` starts, and its text.
async fn turn_end(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    command: Command,
) -> (Option<String>, Outcome, String) {
    handle.send(command).unwrap();
    let (mut error, mut text) = (None, String::new());
    let outcome = loop {
        match next(events).await {
            Event::Error(message) => error = Some(message),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { outcome } => break outcome,
            _ => {}
        }
    };
    (error, outcome, text)
}
#[tokio::test]
async fn opencodes_commands_are_not_read_again_once_a_turn_may_have_changed_them() {
    // opencode 1.18.25 loads its commands once per process and runs that
    // copy whatever its files say later. When the first reading failed and
    // a turn has since run in auto, the files may no longer be what
    // opencode loaded: here the model "rewrote" a command that runs shell
    // into a harmless one. Octet must not read the new text and send it.
    let dir = opencode_workspace(
        "octet-opencode-no-reread",
        &fixture_skill_running("Run !`touch pwned`"),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-fails"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Auto, Limits::default()).await;
    let calls = || std::fs::read_to_string(dir.path().join("debug-calls")).unwrap();
    timeout(Duration::from_secs(5), async {
        while !calls().contains("config") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no reading at connect");
    let (error, _, _) = turn_end(&handle, &mut events, Command::Prompt("hello".into())).await;
    assert_eq!(error, None);
    std::fs::write(
        dir.path().join("fixture-commands.json"),
        fixture_skill_running("Say $ARGUMENTS").to_string(),
    )
    .unwrap();
    std::fs::remove_file(dir.path().join("debug-fails")).unwrap();
    switch_to(&handle, &mut events, Mode::Ask).await;
    let (error, outcome, text) = turn_end(
        &handle,
        &mut events,
        opencode_skill(scenario::OPENCODE_SKILL, "go"),
    )
    .await;
    let error = error.expect("the command was sent");
    assert!(
        error.contains("could not read opencode's commands"),
        "{error}"
    );
    assert!(error.contains("/reconnect"), "{error}");
    assert_eq!((outcome, text.as_str()), (Outcome::Failed, ""));
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_reading_made_while_a_turn_ran_unasked_is_not_trusted() {
    // A turn in auto that edits a command while Octet reads them leaves a
    // book that may differ from what opencode loaded.
    let dir = opencode_workspace(
        "octet-opencode-tainted",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-delay-ms"), "1500").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Auto, Limits::default()).await;
    let (error, _, _) = turn_end(&handle, &mut events, Command::Prompt("hello".into())).await;
    assert_eq!(error, None);
    switch_to(&handle, &mut events, Mode::Ask).await;
    let (error, outcome, text) = turn_end(
        &handle,
        &mut events,
        opencode_skill(scenario::OPENCODE_SKILL, "go"),
    )
    .await;
    let error = error.expect("the command was sent");
    assert!(error.contains("while Octet read them"), "{error}");
    assert_eq!((outcome, text.as_str()), (Outcome::Failed, ""));
    stop(&handle, task).await;
}
#[tokio::test]
async fn a_turn_that_asks_during_the_reading_leaves_it_trusted() {
    // In ask, opencode asks before every edit, so the reading still holds.
    let dir = opencode_workspace(
        "octet-opencode-untainted",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("debug-delay-ms"), "1500").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    let (error, _, _) = turn_end(&handle, &mut events, Command::Prompt("hello".into())).await;
    assert_eq!(error, None);
    let (error, outcome, text) = turn_end(
        &handle,
        &mut events,
        opencode_skill(scenario::OPENCODE_SKILL, "go"),
    )
    .await;
    assert_eq!((error, outcome), (None, Outcome::Completed));
    assert_eq!(text, format!("/{} go", scenario::OPENCODE_SKILL));
    stop(&handle, task).await;
}
/// What Octet says of an opencode `version` its command check is not
/// verified against.
fn unverified(version: &str) -> String {
    format!(
        "opencode {version} is not verified for Octet's command check; commands are off in ask and \
         accept-edits — plain prompts work; /mode auto runs them under opencode's own settings"
    )
}
#[tokio::test]
async fn an_unverified_opencode_runs_no_command_while_the_mode_asks() {
    // The check emulates opencode 1.18.25's internals; another version may
    // run a command otherwise.
    for (reported, shown) in [("1.99.0", "1.99.0"), ("none", "(version not reported)")] {
        let dir = opencode_workspace(
            "octet-opencode-unverified",
            &serde_json::json!({}),
            &serde_json::json!([]),
        );
        std::fs::write(dir.path().join("fixture-version"), reported).unwrap();
        let mut c = opencode();
        c.mode = Mode::Ask;
        c.cwd = dir.path().to_path_buf();
        let (handle, mut events, task) = spawn(c);
        let mut notices = Vec::new();
        loop {
            match next(&mut events).await {
                Event::Notice(notice) => notices.push(notice),
                Event::Ready { .. } => break,
                _ => {}
            }
        }
        assert_eq!(notices, [unverified(shown)], "at connect");
        for command in [
            opencode_skill(scenario::OPENCODE_SKILL, "go"),
            Command::Prompt("/init".into()),
        ] {
            let (error, outcome, text) = turn_end(&handle, &mut events, command).await;
            let error = error.expect("the command was sent");
            assert!(error.ends_with(&unverified(shown)), "{error}");
            assert_eq!((outcome, text.as_str()), (Outcome::Failed, ""));
        }
        // Plain prompts work, and so does opencode's own summary.
        let (error, outcome, _) =
            turn_end(&handle, &mut events, Command::Prompt("hello".into())).await;
        assert_eq!((error, outcome), (None, Outcome::Completed));
        let (error, outcome, _) = turn_end(&handle, &mut events, Command::Compact).await;
        assert_eq!((error, outcome), (None, Outcome::Completed));
        // In auto, opencode's own settings decide.
        switch_to(&handle, &mut events, Mode::Auto).await;
        let (error, outcome, text) = turn_end(
            &handle,
            &mut events,
            opencode_skill(scenario::OPENCODE_SKILL, "go"),
        )
        .await;
        assert_eq!((error, outcome), (None, Outcome::Completed));
        assert_eq!(text, format!("/{} go", scenario::OPENCODE_SKILL));
        stop(&handle, task).await;
    }
}
#[tokio::test]
async fn a_verified_opencode_says_nothing_of_its_version() {
    let (handle, mut events, task) = spawn(opencode());
    loop {
        match next(&mut events).await {
            Event::Notice(notice) => panic!("{notice}"),
            Event::Ready { .. } => break,
            _ => {}
        }
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn lists_that_change_in_a_burst_are_read_once() {
    // Each `opencode debug` run starts opencode, plugins included; lists
    // that change within moments of each other need one reading.
    let dir = opencode_workspace(
        "octet-opencode-burst",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("relist-burst"), "").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let calls = std::fs::read_to_string(dir.path().join("debug-calls")).unwrap();
    assert_eq!(calls.matches("config").count(), 1, "{calls}");
    // The reading is of the last list.
    handle
        .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
        .unwrap();
    assert_eq!(
        turn_text(&mut events).await,
        format!("/{} go", scenario::OPENCODE_SKILL)
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn an_opencode_debug_runs_processes_go_with_it() {
    // `opencode debug` starts opencode, plugins included: a process one
    // starts must not outlive the run, whether it ends or is stopped (here
    // past the check's limit).
    for hangs in [false, true] {
        let dir = opencode_workspace(
            "octet-opencode-debug-children",
            &serde_json::json!({}),
            &serde_json::json!([]),
        );
        std::fs::write(dir.path().join("debug-grandchild"), "").unwrap();
        if hangs {
            std::fs::write(dir.path().join("debug-hangs"), "").unwrap();
        }
        let limits = Limits {
            command_check: Duration::from_millis(500),
            ..Limits::default()
        };
        let (error, outcome, _) = opencode_turn_in(
            &dir,
            Mode::Ask,
            limits,
            opencode_skill(scenario::OPENCODE_SKILL, ""),
        )
        .await;
        let expected = if hangs {
            Outcome::Failed
        } else {
            Outcome::Completed
        };
        assert_eq!(outcome, expected, "hangs: {hangs}: {error:?}");
        let pids: Vec<libc::pid_t> =
            std::fs::read_to_string(dir.path().join("debug-grandchildren"))
                .unwrap()
                .lines()
                .map(|pid| pid.parse().unwrap())
                .collect();
        assert!(!pids.is_empty());
        // SAFETY: signal 0 only checks that the process exists.
        let alive = |pid: &libc::pid_t| unsafe { libc::kill(*pid, 0) == 0 };
        let gone = octet_testkit::wait_until(octet_testkit::QUICK, || !pids.iter().any(alive));
        for pid in &pids {
            // SAFETY: signals only the sleeps this test's fake started.
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        assert!(gone, "hangs: {hangs}: a debug run's process outlived it");
    }
}
#[tokio::test]
async fn lists_that_never_settle_do_not_hold_a_command_forever() {
    // A changed list restarts the reading's settle; lists that change for
    // good must not keep a command waiting for good. Past the bound, the
    // reading under way runs to its end, and is untrusted when the list
    // changed meanwhile.
    let dir = opencode_workspace(
        "octet-opencode-storm",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    std::fs::write(dir.path().join("relist-storm"), "").unwrap();
    std::fs::write(dir.path().join("debug-delay-ms"), "300").unwrap();
    let (handle, mut events, task) = opencode_session_in(&dir, Mode::Ask, Limits::default()).await;
    handle
        .send(opencode_skill(scenario::OPENCODE_SKILL, "go"))
        .unwrap();
    let ended = timeout(Duration::from_secs(8), async {
        let mut error = None;
        loop {
            match events.recv().await {
                Some(Event::Error(message)) => error = Some(message),
                Some(Event::Finished { outcome }) => break (error, outcome),
                Some(_) => {}
                None => panic!("the session ended"),
            }
        }
    })
    .await;
    let (error, outcome) = ended.expect("the command was still held");
    assert_eq!(outcome, Outcome::Failed);
    assert!(
        error
            .as_deref()
            .unwrap_or("")
            .contains("command list changed while Octet read"),
        "{error:?}"
    );
    stop(&handle, task).await;
}
#[test]
fn the_fake_opencode_counts_every_debug_run_though_they_run_at_once() {
    // Octet runs `debug config` and `debug skill` side by side; the fake's
    // count of them must not lose one to the other.
    let dir = opencode_workspace(
        "octet-fake-debug-calls",
        &serde_json::json!({}),
        &serde_json::json!([]),
    );
    let runs: Vec<_> = (0..16)
        .map(|n| {
            std::process::Command::new(octet_testkit::protocol_child())
                .args(["debug", if n % 2 == 0 { "config" } else { "skill" }])
                .current_dir(dir.path())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut run in runs {
        assert!(run.wait().unwrap().success());
    }
    let calls = std::fs::read_to_string(dir.path().join("debug-calls")).unwrap();
    assert_eq!(calls.lines().count(), 16, "{calls}");
}

#[tokio::test]
async fn a_turn_claude_starts_after_a_background_task_is_reported() {
    let (handle, mut events, task) = spawn(claude());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    handle
        .send(Command::Prompt(scenario::BACKGROUND.into()))
        .unwrap();
    let first = until_finished(&mut events).await;
    assert_eq!(text_of(&first), "waiting");
    assert!(
        first.iter().any(|e| matches!(e, Event::Background(1))),
        "the running task is reported: {first:?}"
    );
    // Claude's own turn, after the task ends: it streams like any other.
    let second = until_finished(&mut events).await;
    assert!(
        second.iter().any(|e| matches!(e, Event::Background(0))),
        "the task's end is reported: {second:?}"
    );
    assert!(
        second.iter().any(|e| matches!(e, Event::Started)),
        "{second:?}"
    );
    assert!(
        !second.iter().any(|e| matches!(e, Event::User(_))),
        "nobody typed this turn: {second:?}"
    );
    assert_eq!(text_of(&second), scenario::BACKGROUND_REPLY);
    assert!(matches!(
        second.last(),
        Some(Event::Finished {
            outcome: Outcome::Completed
        })
    ));
    stop(&handle, task).await;
}

/// The fake Antigravity in a temporary folder holding `ws/`, the workspace
/// (with `fixture` as its `fake-agy.json`, unless that is `Null`), and
/// `data/`, Octet's data directory.
fn antigravity(fixture: serde_json::Value) -> (octet_testkit::TempDir, Config) {
    let dir = octet_testkit::TempDir::new("octet-agy");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    if let Some(knobs) = Some(fixture).filter(|fixture| !fixture.is_null()) {
        std::fs::write(ws.join("fake-agy.json"), knobs.to_string()).unwrap();
    }
    let mut c = Config::new(Engine::ANTIGRAVITY, octet_testkit::protocol_child(), &ws);
    c.data_dir = Some(dir.path().join("data"));
    (dir, c)
}
/// The fake Antigravity's `PARAMS` echo, from a session that is ready.
async fn agy_params(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
) -> serde_json::Value {
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    serde_json::from_str(&turn_text(events).await).unwrap()
}
/// A `[configId, value]` pair as the fake's `sets` holds it.
fn set(config: &str, value: &str) -> serde_json::Value {
    serde_json::json!([config, value])
}
/// The `probe-` folders in `data/antigravity`.
fn probe_folders(dir: &octet_testkit::TempDir) -> Vec<String> {
    std::fs::read_dir(dir.path().join("data/antigravity"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("probe-"))
        .collect()
}
/// What Octet says when Antigravity is signed out.
const AGY_SIGN_IN: &str = "Sign in to Antigravity first: run `agy`";
#[tokio::test]
async fn antigravity_connects_lists_models_and_skills() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
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
    assert_eq!(ready.as_deref(), Some("fixture-agy"));
    let models = models.unwrap();
    let flash = models
        .iter()
        .find(|m| m.selection == "gemini-fixture-flash")
        .unwrap_or_else(|| panic!("{models:?}"));
    assert_eq!(flash.name, "Gemini Fixture Flash");
    assert_eq!(flash.id.as_deref(), Some("gemini-fixture-flash"));
    let skills = skills.unwrap();
    assert!(
        skills.iter().any(|s| s.name == scenario::AGY_SKILL),
        "{skills:?}"
    );
    assert_eq!(mode, Some(Mode::Ask));
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_turn_streams_text_and_usage() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
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
        // The thought and the echoed prompt are not the reply.
        assert_eq!(text, scenario::AGY_REPLY);
        assert_eq!(usage, ["42 tokens"]);
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_launches_against_octets_settings() {
    let (dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let echo = agy_params(&handle, &mut events).await;
    // The folder is named by climbing to the root first: `agy` joins the name
    // under its own app data folder.
    let folder = dir.path().join("data/antigravity/review");
    let name = format!(
        "{}{}",
        "../".repeat(32),
        folder.to_str().unwrap().trim_start_matches('/')
    );
    assert_eq!(
        echo["argv"],
        serde_json::json!(["acp", "--cli_dir_name", name])
    );
    let settings = &echo["settings"];
    assert_eq!(settings["toolPermission"], "request-review", "{echo}");
    assert!(settings.get("agentMode").is_none(), "{echo}");
    assert!(settings["permissions"].get("allow").is_none(), "{echo}");
    let sets = echo["sets"].as_array().unwrap();
    assert_eq!(sets.last(), Some(&set("mode", "default")), "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_full_access_launches_with_always_proceed() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.mode = Mode::FullAccess;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::FullAccess))
    })
    .await;
    let echo = agy_params(&handle, &mut events).await;
    let folder = echo["argv"][2].as_str().unwrap();
    assert!(folder.ends_with("antigravity/proceed"), "{echo}");
    assert_eq!(echo["settings"]["toolPermission"], "always-proceed");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_requested_model_and_effort_are_set_and_confirmed() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.model = Some("claude-fixture".into());
    c.effort = Some("low".into());
    let (handle, mut events, task) = spawn(c);
    let selected = wait_for(&mut events, |e| matches!(e, Event::ModelSelected(_))).await;
    assert!(
        matches!(&selected, Event::ModelSelected(model) if model == "claude-fixture"),
        "{selected:?}"
    );
    let effort = wait_for(&mut events, |e| matches!(e, Event::Effort(_))).await;
    assert!(
        matches!(&effort, Event::Effort(Some(level)) if level == "low"),
        "{effort:?}"
    );
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(
        echo["sets"],
        serde_json::json!([
            ["model", "claude-fixture"],
            ["thought_level", "low"],
            ["mode", "default"]
        ])
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_effort_on_a_model_without_levels_is_a_notice() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.model = Some("plain-fixture".into());
    c.effort = Some("high".into());
    let (handle, mut events, task) = spawn(c);
    let (mut noticed, mut dropped) = (false, false);
    loop {
        match next(&mut events).await {
            Event::Notice(notice) if notice == "This Antigravity model takes no thinking level" => {
                noticed = true;
            }
            // The interface stops showing the level asked for.
            Event::Effort(None) if noticed => dropped = true,
            Event::Ready { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(noticed, "the notice came before Ready");
    assert!(dropped, "Effort(None) followed the notice");
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let echo = agy_params(&handle, &mut events).await;
    let sets = echo["sets"].as_array().unwrap();
    assert!(!sets.iter().any(|s| s[0] == "thought_level"), "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_effort_changes_live() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::SetEffort(Some("medium".into())))
        .unwrap();
    let effort = wait_for(&mut events, |e| matches!(e, Event::Effort(_))).await;
    assert!(
        matches!(&effort, Event::Effort(Some(level)) if level == "medium"),
        "{effort:?}"
    );
    handle.send(Command::SetEffort(Some("max".into()))).unwrap();
    let notice = wait_for(
        &mut events,
        |e| matches!(e, Event::Notice(n) if n.starts_with("Antigravity takes effort")),
    )
    .await;
    assert!(
        matches!(&notice, Event::Notice(n) if n.starts_with("Antigravity takes effort low, medium or high")),
        "{notice:?}"
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_resume_gets_its_mode_model_and_effort_back() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.resume = Some("ses-old".into());
    c.mode = Mode::AcceptEdits;
    c.model = Some("claude-fixture".into());
    c.effort = Some("low".into());
    let (handle, mut events, task) = spawn(c);
    let ready = wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    assert!(
        matches!(&ready, Event::Ready { session } if session == "ses-old"),
        "{ready:?}"
    );
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(echo["opened"], "session/resume");
    let sets = echo["sets"].as_array().unwrap();
    for wanted in [
        set("model", "claude-fixture"),
        set("thought_level", "low"),
        set("mode", "accept-edits"),
    ] {
        assert!(sets.contains(&wanted), "{wanted} in {echo}");
    }
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_missing_session_is_an_error_not_a_hang() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.resume = Some(scenario::AGY_MISSING_SESSION.into());
    let (_handle, mut events, task) = spawn(c);
    let error = expect_error(&mut events).await;
    assert!(
        error.starts_with("Antigravity could not open the session: Failed to read session"),
        "{error}"
    );
    ended(task).await;
}
#[tokio::test]
async fn antigravity_signed_out_says_how_to_sign_in() {
    let (_dir, c) = antigravity(serde_json::json!({"signedOut": true}));
    let (_handle, mut events, task) = spawn(c);
    assert_eq!(expect_error(&mut events).await, AGY_SIGN_IN);
    ended(task).await;
}
/// Every event of a session that connects, then runs two plain turns.
async fn agy_two_turns(c: Config) -> Vec<Event> {
    let (handle, mut events, task) = spawn(c);
    let mut seen = Vec::new();
    loop {
        let event = next(&mut events).await;
        let connected = matches!(event, Event::ModeChanged(_));
        seen.push(event);
        if connected {
            break;
        }
    }
    for _ in 0..2 {
        handle.send(Command::Prompt("hello".into())).unwrap();
        seen.extend(until_finished(&mut events).await);
    }
    stop(&handle, task).await;
    seen
}
#[tokio::test]
async fn antigravity_an_unverified_version_is_noticed_once() {
    let unverified = |seen: &[Event]| -> Vec<String> {
        seen.iter()
            .filter_map(|event| match event {
                Event::Notice(n)
                    if n.starts_with("Octet has not been checked against Antigravity") =>
                {
                    Some(n.clone())
                }
                _ => None,
            })
            .collect()
    };
    let (_dir, c) = antigravity(serde_json::json!({"version": "9.9.9"}));
    let notices = unverified(&agy_two_turns(c).await);
    assert_eq!(
        notices,
        [concat!(
            "Octet has not been checked against Antigravity 9.9.9 (checked: 1.5.5). ",
            "If a command or an edit runs without asking in ask or accept-edits, ",
            "Octet cancels the turn and stops sending prompts"
        )]
    );
    let (_dir, c) = antigravity(serde_json::Value::Null);
    assert!(unverified(&agy_two_turns(c).await).is_empty());
}
#[tokio::test]
async fn antigravity_fork_is_refused_before_launch() {
    let (dir, mut c) = antigravity(serde_json::Value::Null);
    c.resume = Some("ses-old".into());
    c.fork = true;
    let (_handle, mut events, task) = spawn(c);
    assert_eq!(
        expect_error(&mut events).await,
        "Antigravity cannot fork a session"
    );
    ended(task).await;
    assert!(!dir.path().join("data/antigravity").exists());
}
#[tokio::test]
async fn antigravity_needs_a_data_directory() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.data_dir = None;
    let (_handle, mut events, task) = spawn(c);
    assert_eq!(
        expect_error(&mut events).await,
        "Antigravity needs Octet's data directory to keep its settings in"
    );
    ended(task).await;
}
#[tokio::test]
async fn antigravity_refuses_the_plugin_switches() {
    for (load, hooks) in [(true, false), (false, true), (false, false)] {
        let (dir, mut c) = antigravity(serde_json::Value::Null);
        c.plugins = octet_engine::live::Plugins { load, hooks };
        let (_handle, mut events, task) = spawn(c);
        assert_eq!(
            expect_error(&mut events).await,
            concat!(
                "Antigravity has no switch to turn plugins or their hooks off; ",
                "start without --no-plugins and --no-plugin-hooks"
            )
        );
        ended(task).await;
        assert!(!dir.path().join("data/antigravity").exists());
    }
}
#[tokio::test]
async fn antigravity_an_untrusted_workspace_does_not_start() {
    let (dir, c) = antigravity(serde_json::Value::Null);
    let agents = dir.path().join("ws/.agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("hooks.json"), "{}").unwrap();
    let (_handle, mut events, task) = spawn(c.clone());
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::Ready { session } => panic!("a session opened: {session}"),
            _ => {}
        }
    };
    assert!(error.contains(".agents/hooks.json"), "{error}");
    ended(task).await;
    // Nothing was launched: Octet's settings were not even written.
    assert!(!dir.path().join("data/antigravity").exists());
    let mut trusted = c;
    trusted.trust_workspace = true;
    let (handle, mut events, task) = spawn(trusted);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_image_goes_inline() {
    let dir = octet_testkit::TempDir::new("octet-image-agy");
    let (_agy, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
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
async fn antigravity_probe_lists_models_and_skills() {
    let (dir, c) = antigravity(serde_json::Value::Null);
    let listing = probe(c, PROBE_LIMIT).await.unwrap();
    assert_eq!(listing.models.len(), 3, "{:?}", listing.models);
    assert!(
        listing.skills.iter().any(|s| s.name == scenario::AGY_SKILL),
        "{:?}",
        listing.skills
    );
    // The fake wrote `left-behind` into its home as it exited; the folder
    // went once it had stopped.
    assert!(dir.path().join("data/antigravity").exists());
    assert_eq!(probe_folders(&dir), Vec::<String>::new());
}
#[tokio::test]
async fn antigravity_a_probe_that_cannot_start_leaves_no_folder() {
    let (dir, mut c) = antigravity(serde_json::Value::Null);
    c.binary = dir.path().join("no-such-agy");
    assert!(probe(c, PROBE_LIMIT).await.is_err());
    assert_eq!(probe_folders(&dir), Vec::<String>::new());
}
/// Sends a plain prompt and returns its error, if any, and its outcome.
async fn agy_turn(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
) -> (Option<String>, Outcome) {
    handle.send(Command::Prompt("hello".into())).unwrap();
    let seen = until_finished(events).await;
    let error = seen.iter().find_map(|event| match event {
        Event::Error(error) => Some(error.clone()),
        _ => None,
    });
    match seen.last() {
        Some(Event::Finished { outcome }) => (error, outcome.clone()),
        other => panic!("the turn did not finish: {other:?}"),
    }
}
/// What a prompt is refused with once the workspace's configuration
/// changed after the start.
const AGY_CHANGED: &str = concat!(
    "Not sent: This workspace's .agents configuration changed since this session started: ",
    ".agents/hooks.json"
);
#[tokio::test]
async fn antigravity_configuration_that_appears_mid_session_stops_the_next_prompt() {
    let (dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    let agents = dir.path().join("ws/.agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("hooks.json"), "{}").unwrap();
    let (error, outcome) = agy_turn(&handle, &mut events).await;
    let error = error.unwrap();
    assert!(error.starts_with(AGY_CHANGED), "{error}");
    assert_eq!(outcome, Outcome::Failed);
    std::fs::remove_dir_all(&agents).unwrap();
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(echo["prompts"], 1, "the refused prompt was not sent");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_trusted_configuration_edited_mid_session_stops_the_next_prompt() {
    let (dir, mut c) = antigravity(serde_json::Value::Null);
    let hooks = dir.path().join("ws/.agents/hooks.json");
    std::fs::create_dir_all(hooks.parent().unwrap()).unwrap();
    std::fs::write(&hooks, "{}").unwrap();
    c.trust_workspace = true;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    std::fs::write(&hooks, r#"{"hooks":[]}"#).unwrap();
    let (error, outcome) = agy_turn(&handle, &mut events).await;
    let error = error.unwrap();
    assert!(error.starts_with(AGY_CHANGED), "{error}");
    assert_eq!(outcome, Outcome::Failed);
    // What was trusted at the start is fine again.
    std::fs::write(&hooks, "{}").unwrap();
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(echo["prompts"], 1, "the refused prompt was not sent");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_configuration_written_as_the_cli_starts_stops_the_first_prompt() {
    let (dir, c) = antigravity(serde_json::json!({"hooksAtStart": true}));
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let (error, outcome) = agy_turn(&handle, &mut events).await;
    let error = error.unwrap();
    assert!(
        error.starts_with("Not sent: This workspace's .agents configuration changed"),
        "{error}"
    );
    assert_eq!(outcome, Outcome::Failed);
    std::fs::remove_dir_all(dir.path().join("ws/.agents")).unwrap();
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(echo["prompts"], 0, "the refused prompt was not sent");
    stop(&handle, task).await;
}
#[test]
fn antigravity_a_second_probe_at_once_is_refused_and_the_first_keeps_its_folder() {
    let (dir, mut c) = antigravity(serde_json::Value::Null);
    c.catalog_only = true;
    let first = octet_engine::live::antigravity_prepare(&c, None).unwrap();
    let scratch = first.scratch.clone().unwrap();
    assert!(scratch.starts_with(dir.path().join("data/antigravity")));
    let second = octet_engine::live::antigravity_prepare(&c, None).unwrap_err();
    assert!(
        second.starts_with("Octet is already asking Antigravity"),
        "{second}"
    );
    assert!(scratch.is_dir(), "the first probe keeps its folder");
}
#[tokio::test]
async fn antigravity_handshake_out_of_order_is_an_error() {
    // Antigravity answers session/new (id 2) before initialize (id 1).
    let (_vendor_dir, vendor) = script_vendor(
        "octet-live-agy-out-of-order",
        "read line\n\
         echo '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"sessionId\":\"s1\",\"configOptions\":[]}}'\n\
         echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\n\
         exec sleep 30\n",
    );
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.binary = vendor;
    let (_handle, mut events, task) = spawn(c);
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

/// `configOptions` with a model, the levels low, medium and high at
/// `level`, and mode `mode` (`None`: no mode option).
fn agy_options(level: &str, mode: Option<&str>) -> serde_json::Value {
    let mut options = vec![
        serde_json::json!({"id": "model", "currentValue": "m", "options": [{"value": "m", "name": "M"}]}),
        serde_json::json!({"id": "thought_level", "currentValue": level,
            "options": [{"value": "low"}, {"value": "medium"}, {"value": "high"}]}),
    ];
    if let Some(mode) = mode {
        options.push(serde_json::json!({"id": "mode", "currentValue": mode,
            "options": [{"value": "default"}, {"value": "plan"}]}));
    }
    serde_json::Value::Array(options)
}
/// A scripted Antigravity, set as `c`'s binary, that runs in `c`'s workspace:
/// for each of `steps` it reads one request, logs it to `sent.log` there,
/// then runs the step (shell that answers it). The guard keeps the script.
fn agy_script(c: &mut Config, steps: &[String]) -> (octet_testkit::TempDir, std::path::PathBuf) {
    let mut body = String::new();
    for step in steps {
        let _ = write!(
            body,
            "read -r line\nprintf '%s\\n' \"$line\" >> sent.log\n{step}\n"
        );
    }
    body.push_str("exec sleep 30\n");
    let (dir, vendor) = script_vendor("octet-live-agy-script", &body);
    c.binary.clone_from(&vendor);
    (dir, vendor)
}
/// The shell line that answers request `id` with `result`.
fn agy_reply(id: u64, result: &serde_json::Value) -> String {
    let frame = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
    format!("echo '{frame}'")
}
/// The requests a scripted Antigravity logged in the workspace of `c`.
fn agy_sent(c: &Config) -> Vec<serde_json::Value> {
    std::fs::read_to_string(c.cwd.join("sent.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
/// What a session stops with when the scripted Antigravity answers its
/// first `mode` request with `reply` (the frame's `result` or `error`
/// member); no `Ready` may come first.
async fn agy_first_mode(reply: serde_json::Value) -> String {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut third = serde_json::json!({"jsonrpc": "2.0", "id": 3});
    third
        .as_object_mut()
        .unwrap()
        .extend(reply.as_object().unwrap().clone());
    let steps = [
        agy_reply(1, &serde_json::json!({"agentInfo": {"version": "1.5.5"}})),
        agy_reply(
            2,
            &serde_json::json!({"sessionId": "s1", "configOptions": agy_options("high", Some("default"))}),
        ),
        format!("echo '{third}'"),
    ];
    let (_script, _) = agy_script(&mut c, &steps);
    let (_handle, mut events, task) = spawn(c);
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::Ready { session } => panic!("a session opened: {session}"),
            Event::ModeChanged(mode) => panic!("the header claimed {mode:?}"),
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    };
    ended(task).await;
    error
}
/// The error that stops a session whose first mode Antigravity would not
/// confirm, for `reason`.
fn agy_mode_refused(reason: &str) -> String {
    format!(
        "Antigravity would not switch to mode default ({reason}); \
         Octet stopped the session so Antigravity never acts outside the mode you chose"
    )
}
#[tokio::test]
async fn antigravity_a_refused_first_mode_stops_the_session() {
    let error = agy_first_mode(
        serde_json::json!({"error": {"code": -32602, "message": "Unknown mode: default"}}),
    )
    .await;
    assert_eq!(error, agy_mode_refused("Unknown mode: default"));
}
#[tokio::test]
async fn antigravity_a_first_mode_reply_naming_no_mode_stops_the_session() {
    let error = agy_first_mode(serde_json::json!({"result": {"configOptions": []}})).await;
    assert_eq!(error, agy_mode_refused("its reply named no mode"));
}
#[tokio::test]
async fn antigravity_a_first_mode_reply_naming_another_mode_stops_the_session() {
    let error = agy_first_mode(
        serde_json::json!({"result": {"configOptions": agy_options("high", Some("plan"))}}),
    )
    .await;
    assert_eq!(error, agy_mode_refused("Antigravity reported mode plan"));
}
#[tokio::test]
async fn antigravity_effort_asked_after_the_effort_step_is_set_once_ready() {
    // The mode's reply is held back until the test has sent the level, so
    // the level arrives after the handshake's effort step has run.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let steps = [
        agy_reply(1, &serde_json::json!({"agentInfo": {"version": "1.5.5"}})),
        agy_reply(
            2,
            &serde_json::json!({"sessionId": "s1", "configOptions": agy_options("high", Some("default"))}),
        ),
        format!(
            ": > asked-mode\nwhile [ ! -e release ]; do sleep 0.05; done\n{}",
            agy_reply(
                3,
                &serde_json::json!({"configOptions": agy_options("high", Some("default"))})
            )
        ),
        agy_reply(
            4,
            &serde_json::json!({"configOptions": agy_options("medium", Some("default"))}),
        ),
    ];
    let (_script, _) = agy_script(&mut c, &steps);
    let ws = c.cwd.clone();
    let (handle, mut events, task) = spawn(c.clone());
    timeout(Duration::from_secs(5), async {
        while !ws.join("asked-mode").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the mode was never asked for");
    handle
        .send(Command::SetEffort(Some("medium".into())))
        .unwrap();
    // The driver takes a queued command before a frame, so the level is
    // recorded before the mode's reply is read.
    std::fs::write(ws.join("release"), "").unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    let effort = wait_for(&mut events, |e| matches!(e, Event::Effort(_))).await;
    assert!(
        matches!(&effort, Event::Effort(Some(level)) if level == "medium"),
        "{effort:?}"
    );
    let sent = agy_sent(&c);
    let last = sent.last().unwrap();
    assert_eq!(last["method"], "session/set_config_option", "{sent:?}");
    assert_eq!(last["params"]["configId"], "thought_level");
    assert_eq!(last["params"]["value"], "medium");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_session_reply_without_options_still_gets_the_level_asked_for() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.effort = Some("low".into());
    let steps = [
        agy_reply(1, &serde_json::json!({"agentInfo": {"version": "1.5.5"}})),
        agy_reply(2, &serde_json::json!({"sessionId": "s1"})),
        agy_reply(
            3,
            &serde_json::json!({"configOptions": agy_options("low", Some("default"))}),
        ),
        agy_reply(
            4,
            &serde_json::json!({"configOptions": agy_options("low", Some("default"))}),
        ),
    ];
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::Ready { .. })).await;
    let effort = wait_for(&mut events, |e| matches!(e, Event::Effort(_))).await;
    assert!(
        matches!(&effort, Event::Effort(Some(level)) if level == "low"),
        "{effort:?}"
    );
    let sent = agy_sent(&c);
    let level = &sent[2];
    assert_eq!(level["params"]["configId"], "thought_level", "{sent:?}");
    assert_eq!(level["params"]["value"], "low");
    stop(&handle, task).await;
}

/// The text and tool calls of an `APPROVAL` turn, and the notices it gave,
/// from a session of `c`; its approval is answered with `answer`, or left
/// for its window to close when that is `None`.
async fn agy_answered(c: Config, answer: Option<bool>) -> (String, Vec<ToolCall>, Vec<String>) {
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let (mut text, mut tools, mut notices) = (String::new(), Vec::new(), Vec::new());
    loop {
        match next(&mut events).await {
            Event::Approval { id, summary, .. } => {
                assert_eq!(summary.action, "Run a shell command");
                assert_eq!(summary.command.as_deref(), Some("echo fixture"));
                if let Some(allow) = answer {
                    handle.send(Command::Answer { id, allow }).unwrap();
                }
            }
            Event::Text(t) => text.push_str(&t),
            Event::Tool(call) => tools.push(call),
            Event::Notice(notice) => notices.push(notice),
            Event::Finished { outcome } => {
                assert_eq!(outcome, Outcome::Completed);
                break;
            }
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    stop(&handle, task).await;
    (text, tools, notices)
}
#[tokio::test]
async fn antigravity_approval_allowed_sends_allow() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (text, tools, _) = agy_answered(c, Some(true)).await;
    assert_eq!(text, "allow");
    assert_eq!(
        tools.last().map(|t| t.phase),
        Some(ToolPhase::Done),
        "{tools:?}"
    );
}
#[tokio::test]
async fn antigravity_approval_denied_sends_deny() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (text, tools, _) = agy_answered(c, Some(false)).await;
    assert_eq!(text, "deny");
    assert_eq!(
        tools.last().map(|t| t.phase),
        Some(ToolPhase::Failed),
        "{tools:?}"
    );
}
#[tokio::test]
async fn antigravity_approval_times_out_as_denied() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.approval_timeout = Duration::from_millis(200);
    let (text, tools, notices) = agy_answered(c, None).await;
    assert_eq!(text, "deny");
    assert_eq!(
        tools.last().map(|t| t.phase),
        Some(ToolPhase::Failed),
        "{tools:?}"
    );
    assert!(
        notices
            .iter()
            .any(|n| n == "Approval timed out and was denied"),
        "{notices:?}"
    );
}
#[tokio::test]
async fn antigravity_nine_approvals_deny_the_ninth() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
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
            // The ninth request was denied: the fake echoes the answer.
            Event::Text(text) if text == r#"answered "9":deny"# => break,
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
        |e| matches!(e, Event::Text(t) if t == r#"answered "1":allow"#),
    )
    .await;
    handle.interrupt();
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_an_edit_asks_and_names_its_file() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::AGY_EDIT.into()))
        .unwrap();
    let Event::Approval { id, summary, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(summary.action, "Edit files");
    assert!(
        summary.files.iter().any(|f| f.ends_with("/fixture.txt")),
        "{:?}",
        summary.files
    );
    handle.send(Command::Answer { id, allow: false }).unwrap();
    assert_eq!(turn_text(&mut events).await, "deny");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_cancel_is_interrupted() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
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
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    stop(&handle, task).await;
}
/// The steps of a scripted Antigravity's handshake: it opens session `s1`
/// and confirms the first mode, `default`.
fn agy_handshake() -> Vec<String> {
    vec![
        agy_reply(1, &serde_json::json!({"agentInfo": {"version": "1.5.5"}})),
        agy_reply(
            2,
            &serde_json::json!({"sessionId": "s1", "configOptions": agy_options("high", Some("default"))}),
        ),
        agy_reply(
            3,
            &serde_json::json!({"configOptions": agy_options("high", Some("default"))}),
        ),
    ]
}
/// The shell line that sends the permission request `id` for `session`, as
/// Antigravity asks to run `echo fixture`, with its two options.
fn agy_permission(session: &str, id: &str) -> String {
    let request = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {"sessionId": session,
            "toolCall": {"toolCallId": "call-1", "name": "run_command", "kind": "execute",
                "title": "echo fixture", "rawInput": {"command": "echo fixture"}, "status": "pending"},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "deny", "name": "Deny", "kind": "reject_once"}]}});
    format!("echo '{request}'")
}
/// The reply rejecting request `id`, as Octet sends it.
fn agy_denial(id: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id,
        "result": {"outcome": {"outcome": "selected", "optionId": "deny"}}})
}
#[tokio::test]
async fn antigravity_cancel_denies_a_waiting_approval() {
    // The scripted Antigravity asks, then reads the next two frames before it
    // ends the prompt `cancelled`: Octet's cancel and its denial, in order.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut steps = agy_handshake();
    steps.extend([
        agy_permission("s1", "1"),
        ":".to_owned(),
        agy_reply(4, &serde_json::json!({"stopReason": "cancelled"})),
        agy_reply(5, &serde_json::json!({"stopReason": "end_turn"})),
    ]);
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    handle.interrupt();
    let (mut closed, mut text) = (false, String::new());
    let outcome = loop {
        match next(&mut events).await {
            Event::ApprovalClosed(done) if done == id => closed = true,
            Event::Text(t) => text.push_str(&t),
            Event::Finished { outcome } => break outcome,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert!(closed, "the approval was closed");
    assert_eq!(outcome, Outcome::Interrupted);
    assert_eq!(text, "");
    let sent = agy_sent(&c);
    assert_eq!(
        sent[4..6],
        [
            serde_json::json!({"jsonrpc": "2.0", "method": "session/cancel",
                "params": {"sessionId": "s1"}}),
            agy_denial("1"),
        ],
        "{sent:?}"
    );
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    stop(&handle, task).await;
}
/// Switches a ready session to `target`; the first mode event or notice
/// that follows must confirm it.
async fn agy_switch(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    target: Mode,
) {
    handle.send(Command::SetMode(target)).unwrap();
    let changed = wait_for(events, |e| {
        matches!(e, Event::ModeChanged(_) | Event::Notice(_))
    })
    .await;
    assert!(
        matches!(&changed, Event::ModeChanged(mode) if *mode == target),
        "{changed:?}"
    );
}
#[tokio::test]
async fn antigravity_live_switch_sets_the_mode() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    agy_switch(&handle, &mut events, Mode::AcceptEdits).await;
    handle
        .send(Command::Prompt(scenario::AGY_EDIT.into()))
        .unwrap();
    let mut text = String::new();
    loop {
        match next(&mut events).await {
            Event::Approval { detail, .. } => panic!("the edit asked: {detail}"),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(text, "wrote");
    let echo = agy_params(&handle, &mut events).await;
    let sets = echo["sets"].as_array().unwrap();
    assert_eq!(sets.last(), Some(&set("mode", "accept-edits")), "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_auto_runs_a_command_unasked_without_tripping() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    agy_switch(&handle, &mut events, Mode::Auto).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let mut text = String::new();
    let outcome = loop {
        match next(&mut events).await {
            Event::Approval { detail, .. } => panic!("the command asked: {detail}"),
            Event::Text(t) => text.push_str(&t),
            Event::Finished { outcome } => break outcome,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(text, "ran");
    assert_eq!(outcome, Outcome::Completed);
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_refused_switch_keeps_the_mode() {
    let (_dir, c) = antigravity(serde_json::json!({"rejectSwitches": true}));
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    let notice = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&notice, Event::Notice(text) if text.starts_with("Antigravity did not switch to accept-edits")),
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
async fn antigravity_a_prompt_waits_for_an_unanswered_switch_and_is_not_sent() {
    let (_dir, c) = antigravity(serde_json::json!({"hangSwitches": true}));
    let (handle, mut events, task) = spawn_with_limits(c, quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    handle.send(Command::Prompt("hello".into())).unwrap();
    let mut notices = Vec::new();
    let error = loop {
        match next(&mut events).await {
            Event::Notice(notice) => notices.push(notice),
            Event::Error(error) => break error,
            Event::Started => notices.push("started".to_owned()),
            _ => {}
        }
    };
    let waiting = notices
        .iter()
        .position(|n| n.starts_with("Waiting for Antigravity"));
    let started = notices.iter().position(|n| n == "started");
    assert!(
        waiting < started,
        "the prompt waited before it started: {notices:?}"
    );
    assert!(
        notices.contains(
            &"Waiting for Antigravity to confirm the switch to accept-edits before sending your prompt"
                .to_owned()
        ),
        "{notices:?}"
    );
    assert!(error.starts_with("Not sent: "), "{error}");
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
    // The switch is still unanswered, so the mode Antigravity runs is unknown.
    let (error, outcome) = agy_turn(&handle, &mut events).await;
    let error = error.unwrap();
    assert!(error.contains("its mode is unknown"), "{error}");
    assert_eq!(outcome, Outcome::Failed);
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_late_confirmation_is_applied() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    // The confirmation comes 1.5 s after the 1 s window closes.
    let confirmed = serde_json::json!({"configOptions": agy_options("high", Some("accept-edits"))});
    let mut steps = agy_handshake();
    steps.push(format!("sleep 2.5\n{}", agy_reply(4, &confirmed)));
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn_with_limits(c, quick());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    let mut seen = Vec::new();
    while seen.last().map(String::as_str) != Some("mode accept-edits") {
        match wait_for_within(&mut events, 5, |_| true).await {
            Event::Notice(text) => seen.push(text),
            Event::ModeChanged(mode) => seen.push(format!("mode {}", mode.label())),
            _ => {}
        }
    }
    assert_eq!(
        seen,
        [
            "Antigravity has not confirmed the switch to accept-edits",
            "mode ask",
            "Antigravity confirmed the switch to accept-edits late",
            "mode accept-edits",
        ]
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_another_sessions_permission_is_rejected_not_shown() {
    // The scripted Antigravity asks for another session, then reads Octet's
    // reply before it ends the prompt.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut steps = agy_handshake();
    steps.extend([
        agy_permission("other-ses", "other-1"),
        agy_reply(4, &serde_json::json!({"stopReason": "end_turn"})),
    ]);
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let mut notices = Vec::new();
    loop {
        match next(&mut events).await {
            Event::Approval { detail, .. } => panic!("another session's request shown: {detail}"),
            Event::Notice(notice) => notices.push(notice),
            Event::Finished { .. } => break,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
    let asked: Vec<&String> = notices
        .iter()
        .filter(|n| n.starts_with("Antigravity asked for a permission outside"))
        .collect();
    assert_eq!(
        asked,
        [
            "Antigravity asked for a permission outside this session's running turn; Octet rejected it"
        ],
        "{notices:?}"
    );
    let sent = agy_sent(&c);
    assert_eq!(sent.get(4), Some(&agy_denial("other-1")), "{sent:?}");
    stop(&handle, task).await;
}
/// A scripted Antigravity that opens session `s1` (listing `commands` after
/// its reply, when there are any), confirms the first mode, then ends each
/// of `turns` prompts `end_turn`.
fn agy_scripted_turns(
    c: &mut Config,
    commands: &[&str],
    turns: u64,
) -> (octet_testkit::TempDir, std::path::PathBuf) {
    let mut steps = agy_handshake();
    if !commands.is_empty() {
        let listed: Vec<serde_json::Value> = commands
            .iter()
            .map(|name| serde_json::json!({"name": name, "description": "A command"}))
            .collect();
        let update = serde_json::json!({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "s1",
            "update": {"sessionUpdate": "available_commands_update", "availableCommands": listed}}});
        let _ = write!(steps[1], "\necho '{update}'");
    }
    steps.extend(
        (4..4 + turns).map(|id| agy_reply(id, &serde_json::json!({"stopReason": "end_turn"}))),
    );
    agy_script(c, &steps)
}
/// The text of the prompt request `frame`, when it is one text block.
fn agy_prompt_text(frame: &serde_json::Value) -> &str {
    assert_eq!(frame["method"], "session/prompt", "{frame}");
    let prompt = frame["params"]["prompt"].as_array().unwrap();
    assert_eq!(prompt.len(), 1, "{frame}");
    assert_eq!(prompt[0]["type"], "text", "{frame}");
    prompt[0]["text"].as_str().unwrap()
}
#[tokio::test]
async fn antigravity_skill_runs_as_a_slash_prompt() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let (_script, _) = agy_scripted_turns(&mut c, &[scenario::AGY_SKILL], 3);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let skill = |args: &str, context: Option<&str>| Command::Skill {
        name: scenario::AGY_SKILL.into(),
        path: None,
        args: args.into(),
        display: format!("/{} {args}", scenario::AGY_SKILL),
        context: context.map(str::to_owned),
    };
    for command in [
        skill("go", None),
        skill("go", Some("earlier conversation")),
        skill("", None),
    ] {
        handle.send(command).unwrap();
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
    }
    let sent = agy_sent(&c);
    let texts: Vec<&str> = sent[3..].iter().map(agy_prompt_text).collect();
    assert_eq!(
        texts,
        [
            format!("/{} go", scenario::AGY_SKILL),
            format!("earlier conversation\n\n/{} go", scenario::AGY_SKILL),
            format!("/{}", scenario::AGY_SKILL),
        ]
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_compact_uses_its_command() {
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let (_script, _) = agy_scripted_turns(&mut c, &["compact"], 1);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Compact).unwrap();
    let mut compacted = false;
    let outcome = loop {
        match next(&mut events).await {
            Event::Notice(n) if n == "Context compacted" => compacted = true,
            Event::Finished { outcome } => break outcome,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert!(compacted, "the compaction was announced");
    assert_eq!(outcome, Outcome::Completed);
    let sent = agy_sent(&c);
    assert_eq!(agy_prompt_text(&sent[3]), "/compact");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_compact_is_refused_where_the_session_has_no_such_command() {
    let (_dir, c) = antigravity(serde_json::json!({"noCompact": true}));
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::Skills(_))).await;
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle.send(Command::Compact).unwrap();
    let error = expect_error(&mut events).await;
    assert_eq!(
        error,
        "Antigravity does not offer compaction in this session"
    );
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
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(echo["prompts"], 0, "the compaction was not sent");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_switch_reply_naming_no_mode_stops_the_session() {
    let (_dir, c) = antigravity(serde_json::json!({"bareSwitchReply": true}));
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::SetMode(Mode::AcceptEdits)).unwrap();
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::ModeChanged(Mode::AcceptEdits) => panic!("the header claimed accept-edits"),
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    };
    assert!(
        error.starts_with(
            "Antigravity would not switch to mode accept-edits (its reply named no mode)"
        ),
        "{error}"
    );
    // The session ends by itself.
    ended(task).await;
    drop(handle);
}
#[tokio::test]
async fn antigravity_failed_turn_shows_the_message() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
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
async fn antigravity_tool_error_is_a_failed_call() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::TOOL_FAIL.into()))
        .unwrap();
    let mut calls = Vec::new();
    let outcome = loop {
        match next(&mut events).await {
            Event::Tool(call) => calls.push((call.id, call.name, call.phase)),
            Event::Finished { outcome } => break outcome,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    };
    let call = |phase| ("call-1".to_owned(), "read_file".to_owned(), phase);
    assert_eq!(calls, [call(ToolPhase::Running), call(ToolPhase::Failed)]);
    assert_eq!(outcome, Outcome::Completed);
    stop(&handle, task).await;
}

/// The errors among `seen`.
fn errors_of(seen: &[Event]) -> Vec<String> {
    seen.iter()
        .filter_map(|event| match event {
            Event::Error(error) => Some(error.clone()),
            _ => None,
        })
        .collect()
}
/// The notices among `seen`.
fn notices_of(seen: &[Event]) -> Vec<String> {
    seen.iter()
        .filter_map(|event| match event {
            Event::Notice(notice) => Some(notice.clone()),
            _ => None,
        })
        .collect()
}
/// How the turn `seen` ended.
fn outcome_of(seen: &[Event]) -> Outcome {
    match seen.last() {
        Some(Event::Finished { outcome }) => outcome.clone(),
        other => panic!("the turn did not finish: {other:?}"),
    }
}
/// Every event of the turn the prompt `text` starts in a ready session.
async fn agy_run(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    text: &str,
) -> Vec<Event> {
    handle.send(Command::Prompt(text.into())).unwrap();
    until_finished(events).await
}
/// The fake Antigravity, logging each prompt it receives to `prompts.log`
/// in its workspace, in `mode`.
fn agy_logging(mode: Mode) -> (octet_testkit::TempDir, Config) {
    let (dir, mut c) = antigravity(serde_json::json!({"logPrompts": true}));
    c.mode = mode;
    (dir, c)
}
/// The prompts the logging fake of `c` received, in order.
fn agy_prompts(c: &Config) -> Vec<String> {
    std::fs::read_to_string(c.cwd.join("prompts.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
/// The tripwire's error for an unasked `echo fixture` under `label`.
fn agy_ran_unasked(label: &str) -> String {
    format!(
        "Antigravity ran a command without asking while Octet shows {label}: echo fixture. \
         Octet's settings did not take effect with this agy, so ask and accept-edits cannot be relied on. \
         Octet cancelled the turn and sends no more prompts in this session. What ran cannot be undone"
    )
}
/// What every turn is refused with once Antigravity acted unasked.
const AGY_ACTED: &str = "Not sent: Antigravity acted without asking earlier in this session, \
                         so Octet sends it nothing more. Start a new session";
/// Checks that a tripped session refuses a prompt, `PARAMS`, a compaction
/// and a skill, unsent: the logging fake of `c` received only `first`.
async fn agy_sends_nothing_more(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    c: &Config,
    first: &str,
) {
    let skill = Command::Skill {
        name: scenario::AGY_SKILL.into(),
        path: None,
        args: String::new(),
        display: format!("/{}", scenario::AGY_SKILL),
        context: None,
    };
    for command in [
        Command::Prompt("hello".into()),
        Command::Prompt(scenario::PARAMS.into()),
        Command::Compact,
        skill,
    ] {
        handle.send(command).unwrap();
        let seen = until_finished(events).await;
        assert_eq!(errors_of(&seen), [AGY_ACTED], "{seen:?}");
        assert_eq!(outcome_of(&seen), Outcome::Failed);
    }
    assert_eq!(agy_prompts(c), [first], "nothing more reached Antigravity");
}
#[tokio::test]
async fn antigravity_a_command_that_runs_unasked_stops_the_session() {
    let (_dir, c) = agy_logging(Mode::Ask);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_UNASKED).await;
    let errors = errors_of(&seen);
    assert_eq!(errors, [agy_ran_unasked("ask")], "{seen:?}");
    assert!(errors[0].starts_with(
        "Antigravity ran a command without asking while Octet shows ask: echo fixture"
    ));
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    agy_sends_nothing_more(&handle, &mut events, &c, scenario::AGY_UNASKED).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_an_unasked_command_trips_in_accept_edits_too() {
    let (_dir, c) = agy_logging(Mode::AcceptEdits);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_UNASKED).await;
    assert_eq!(
        errors_of(&seen),
        [agy_ran_unasked("accept-edits")],
        "{seen:?}"
    );
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    agy_sends_nothing_more(&handle, &mut events, &c, scenario::AGY_UNASKED).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_an_unasked_edit_is_fine_in_accept_edits() {
    let (_dir, c) = agy_logging(Mode::AcceptEdits);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_EDIT).await;
    assert_eq!(errors_of(&seen), Vec::<String>::new(), "{seen:?}");
    assert_eq!(text_of(&seen), "wrote");
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    // The session goes on.
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    assert_eq!(agy_prompts(&c), [scenario::AGY_EDIT, "hello"]);
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_an_edit_begun_under_accept_edits_does_not_trip_after_a_switch_to_ask() {
    // The edit starts under accept-edits; the switch to ask is confirmed
    // while it runs, and only then does it complete, unasked.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    c.mode = Mode::AcceptEdits;
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| {
        matches!(e, Event::ModeChanged(Mode::AcceptEdits))
    })
    .await;
    handle
        .send(Command::Prompt(scenario::AGY_SLOW_EDIT.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Tool(_))).await;
    handle.send(Command::SetMode(Mode::Ask)).unwrap();
    let seen = until_finished(&mut events).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::ModeChanged(Mode::Ask))),
        "{seen:?}"
    );
    assert_eq!(errors_of(&seen), Vec::<String>::new(), "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_denied_command_does_not_trip() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    handle
        .send(Command::Prompt(scenario::APPROVAL.into()))
        .unwrap();
    let Event::Approval { id, .. } =
        wait_for(&mut events, |e| matches!(e, Event::Approval { .. })).await
    else {
        unreachable!()
    };
    handle.send(Command::Answer { id, allow: false }).unwrap();
    let seen = until_finished(&mut events).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Tool(call) if call.phase == ToolPhase::Failed)),
        "the command ended failed: {seen:?}"
    );
    assert_eq!(errors_of(&seen), Vec::<String>::new(), "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    // Not tripped: the next prompt is sent.
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_command_begun_after_tightening_to_ask_trips() {
    // The command starts only once the switch to ask is confirmed, so it is
    // judged by ask although the turn began under auto.
    let (_dir, c) = agy_logging(Mode::Auto);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Auto))).await;
    handle
        .send(Command::Prompt(scenario::AGY_LATE_COMMAND.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Started)).await;
    handle.send(Command::SetMode(Mode::Ask)).unwrap();
    let seen = until_finished(&mut events).await;
    let switched = seen
        .iter()
        .position(|e| matches!(e, Event::ModeChanged(Mode::Ask)));
    let tripped = seen.iter().position(|e| matches!(e, Event::Error(_)));
    assert!(switched.is_some() && switched < tripped, "{seen:?}");
    let errors = errors_of(&seen);
    assert!(
        errors[0].starts_with("Antigravity ran a command without asking while Octet shows ask"),
        "{errors:?}"
    );
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    agy_sends_nothing_more(&handle, &mut events, &c, scenario::AGY_LATE_COMMAND).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_more_tool_calls_than_octet_checks_cancels_the_turn() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_FLOOD).await;
    let errors = errors_of(&seen);
    assert_eq!(
        errors,
        [
            "Antigravity made more than 1024 tool calls in one turn, more than Octet checks for unasked actions. \
             Octet cancelled the turn"
        ],
        "{errors:?}"
    );
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    // The session goes on.
    assert_eq!(
        agy_turn(&handle, &mut events).await,
        (None, Outcome::Completed)
    );
    // In auto the calls are shown, not remembered, and the turn completes.
    agy_switch(&handle, &mut events, Mode::Auto).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_FLOOD).await;
    assert_eq!(errors_of(&seen), Vec::<String>::new());
    assert_eq!(text_of(&seen), scenario::AGY_REPLY);
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_turn_octet_cancels_ends_only_with_its_own_reply() {
    // After Octet's cancel, the fake answers the set-back, says `late`, and
    // only then answers the prompt.
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_TO_AUTO).await;
    assert!(text_of(&seen).contains("late"), "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    // The next turn's events hold one end, its own, after its reply.
    let next_turn = agy_run(&handle, &mut events, "hello").await;
    assert!(
        matches!(next_turn.first(), Some(Event::User(_))),
        "no end of the earlier turn came first: {next_turn:?}"
    );
    assert_eq!(text_of(&next_turn), scenario::AGY_REPLY);
    assert_eq!(outcome_of(&next_turn), Outcome::Completed);
    stop(&handle, task).await;
}
/// The notice Octet gives when Antigravity moves itself to `plan` under ask.
const AGY_TO_PLAN_NOTICE: &str =
    "Antigravity moved to its plan mode. Octet sets ask back before the next prompt";
/// The `mode` values the fake was set to, in order, from its `PARAMS` echo.
fn agy_mode_sets(echo: &serde_json::Value) -> Vec<String> {
    echo["sets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|pair| pair[0] == "mode")
        .map(|pair| pair[1].as_str().unwrap().to_owned())
        .collect()
}
#[tokio::test]
async fn antigravity_moving_itself_to_plan_while_idle_is_put_back_at_once() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_IDLE_PLAN).await;
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    let notice = wait_for(&mut events, |e| {
        matches!(e, Event::Notice(_) | Event::ModeChanged(_))
    })
    .await;
    assert!(
        matches!(&notice, Event::Notice(text) if text == AGY_TO_PLAN_NOTICE),
        "{notice:?}"
    );
    let echo = agy_params(&handle, &mut events).await;
    // The handshake's mode, then the set-back.
    assert_eq!(agy_mode_sets(&echo), ["default", "default"], "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_moving_itself_to_plan_is_put_back_before_the_next_prompt() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_TO_PLAN).await;
    assert_eq!(notices_of(&seen), [AGY_TO_PLAN_NOTICE], "{seen:?}");
    assert!(
        !seen.iter().any(|e| matches!(e, Event::ModeChanged(_))),
        "the header keeps Octet's mode: {seen:?}"
    );
    assert_eq!(text_of(&seen), scenario::AGY_REPLY);
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    handle
        .send(Command::Prompt(scenario::PARAMS.into()))
        .unwrap();
    let params = until_finished(&mut events).await;
    assert!(
        !params.iter().any(|e| matches!(e, Event::ModeChanged(_))),
        "the set-back's confirmation shows nothing: {params:?}"
    );
    let echo: serde_json::Value = serde_json::from_str(&text_of(&params)).unwrap();
    assert_eq!(agy_mode_sets(&echo), ["default", "default"], "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_moving_itself_to_a_looser_mode_cancels_the_turn() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_TO_AUTO).await;
    let errors = errors_of(&seen);
    assert_eq!(
        errors,
        [
            "Antigravity moved to its auto mode while Octet shows ask. Octet cancelled the turn and set ask back"
        ],
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::ModeChanged(_))),
        "the header keeps Octet's mode: {seen:?}"
    );
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    let echo = agy_params(&handle, &mut events).await;
    assert_eq!(agy_mode_sets(&echo), ["default", "default"], "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_model_it_changes_itself_reaches_the_header() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(_))).await;
    // The first model opened at high; low is set before the change.
    handle.send(Command::SetEffort(Some("low".into()))).unwrap();
    wait_for(
        &mut events,
        |e| matches!(e, Event::Effort(Some(level)) if level == "low"),
    )
    .await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_NEW_MODEL).await;
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::ModelSelected(model) if model == "claude-fixture")),
        "{seen:?}"
    );
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    // The levels were read again from the change: the new model opened at
    // low, so `/effort default` has nothing to send. Read from before the
    // change, it would set the first model's high.
    handle.send(Command::SetEffort(None)).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Effort(None))).await;
    let echo = agy_params(&handle, &mut events).await;
    let levels: Vec<&serde_json::Value> = echo["sets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|pair| pair[0] == "thought_level")
        .map(|pair| &pair[1])
        .collect();
    assert_eq!(levels, ["low"], "{echo}");
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_permission_asked_while_cancelling_does_not_trip() {
    // The fake asks once it reads the cancel, and fails the command only
    // after it reads Octet's reply to that request: the failed frame, and
    // the turn's end, prove the rejection reached the wire. The request was
    // made in `Phase::Interrupting`; had it not marked the call asked, the
    // failed command would trip the session and the next prompt would not
    // be sent.
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle
        .send(Command::Prompt(scenario::AGY_ASK_ON_CANCEL.into()))
        .unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Tool(_))).await;
    handle.interrupt();
    let seen = until_finished(&mut events).await;
    let asked: Vec<String> = notices_of(&seen)
        .into_iter()
        .filter(|n| {
            n.starts_with("Antigravity asked for a permission while its turn is being cancelled")
        })
        .collect();
    assert_eq!(asked.len(), 1, "{seen:?}");
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::Tool(call) if call.phase == ToolPhase::Failed)),
        "the command failed after the rejection: {seen:?}"
    );
    assert!(!text_of(&seen).contains("allowed on cancel"), "{seen:?}");
    assert_eq!(errors_of(&seen), Vec::<String>::new(), "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Interrupted);
    let next_turn = agy_run(&handle, &mut events, "hello").await;
    assert_eq!(text_of(&next_turn), scenario::AGY_REPLY);
    assert_eq!(outcome_of(&next_turn), Outcome::Completed);
    stop(&handle, task).await;
}
/// What Octet says when Antigravity starts a subagent in an asking mode.
const AGY_SUBAGENT_NOTICE: &str = concat!(
    "Antigravity started a subagent. Its approval requests never reach Octet, so what it must ask for ",
    "waits and does not run. In auto, Antigravity's own checker decides"
);
#[tokio::test]
async fn antigravity_a_subagent_is_reported_once_in_the_asking_modes() {
    let (_dir, c) = antigravity(serde_json::Value::Null);
    let (handle, mut events, task) = spawn(c);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    // Once a turn: the call's two frames give one notice, each turn its own.
    for _ in 0..2 {
        let seen = agy_run(&handle, &mut events, scenario::AGY_SUBAGENT).await;
        let started: Vec<String> = notices_of(&seen)
            .into_iter()
            .filter(|n| n.starts_with("Antigravity started a subagent"))
            .collect();
        assert_eq!(started, [AGY_SUBAGENT_NOTICE], "{seen:?}");
        assert_eq!(outcome_of(&seen), Outcome::Completed);
    }
    agy_switch(&handle, &mut events, Mode::Auto).await;
    let seen = agy_run(&handle, &mut events, scenario::AGY_SUBAGENT).await;
    assert!(
        !notices_of(&seen)
            .iter()
            .any(|n| n.starts_with("Antigravity started a subagent")),
        "{seen:?}"
    );
    assert_eq!(outcome_of(&seen), Outcome::Completed);
    stop(&handle, task).await;
}
/// The shell line that sends `update` as a `session/update` for session
/// `s1`.
fn agy_update(update: &serde_json::Value) -> String {
    let frame = serde_json::json!({"jsonrpc": "2.0", "method": "session/update",
        "params": {"sessionId": "s1", "update": update}});
    format!("echo '{frame}'")
}
/// The scripted `tool_call` that starts `echo fixture` as `call-1`.
fn agy_command_started() -> String {
    agy_update(
        &serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "call-1",
        "name": "run_command", "kind": "execute", "title": "echo fixture",
        "rawInput": {"command": "echo fixture"}, "status": "in_progress"}),
    )
}
/// The scripted `tool_call_update` that says `call-1` completed.
fn agy_command_completed() -> String {
    agy_update(
        &serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1",
        "status": "completed"}),
    )
}
/// Checks that a tripped session on a scripted Antigravity refuses the next
/// prompt, unsent: the vendor received one `session/prompt` only.
async fn agy_scripted_sends_nothing_more(
    handle: &octet_engine::live::Handle,
    events: &mut mpsc::Receiver<Event>,
    c: &Config,
) {
    let seen = agy_run(handle, events, "hello").await;
    assert_eq!(errors_of(&seen), [AGY_ACTED], "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    let sent = agy_sent(c);
    let prompts = sent
        .iter()
        .filter(|frame| frame["method"] == "session/prompt")
        .count();
    assert_eq!(prompts, 1, "{sent:?}");
}
#[tokio::test]
async fn antigravity_a_command_kind_named_only_on_a_later_frame_trips() {
    // The call's first frame names no kind; the frame that ends it says it
    // was a command.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut steps = agy_handshake();
    steps.extend([
        format!(
            "{}\n{}",
            agy_update(
                &serde_json::json!({"sessionUpdate": "tool_call", "toolCallId": "c1",
                "title": "Run", "status": "pending"})
            ),
            agy_update(
                &serde_json::json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
                "kind": "execute", "rawInput": {"command": "rm x"}, "status": "completed"})
            )
        ),
        agy_reply(4, &serde_json::json!({"stopReason": "cancelled"})),
    ]);
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, "hello").await;
    let expected = agy_ran_unasked("ask").replace(": echo fixture.", ": rm x.");
    assert_eq!(errors_of(&seen), [expected], "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    assert_eq!(agy_sent(&c)[4]["method"], "session/cancel");
    agy_scripted_sends_nothing_more(&handle, &mut events, &c).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_command_that_ends_unasked_after_the_users_interrupt_is_still_reported() {
    // The command completes only once the user's cancel has arrived.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut steps = agy_handshake();
    steps.extend([
        agy_command_started(),
        format!(
            "{}\n{}",
            agy_command_completed(),
            agy_reply(4, &serde_json::json!({"stopReason": "cancelled"}))
        ),
    ]);
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    wait_for(&mut events, |e| matches!(e, Event::Tool(_))).await;
    handle.interrupt();
    let seen = until_finished(&mut events).await;
    let errors = errors_of(&seen);
    assert_eq!(errors, [agy_ran_unasked("ask")], "{seen:?}");
    assert!(errors[0].starts_with("Antigravity ran a command without asking"));
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    let sent = agy_sent(&c);
    let cancels = sent
        .iter()
        .filter(|frame| frame["method"] == "session/cancel")
        .count();
    assert_eq!(cancels, 1, "no second cancel: {sent:?}");
    agy_scripted_sends_nothing_more(&handle, &mut events, &c).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_a_command_that_ends_unasked_after_a_looser_mode_cancel_is_still_reported() {
    // Antigravity moves to auto while a command begun under ask runs: Octet
    // cancels and sets ask back; then the command completes unasked.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut steps = agy_handshake();
    steps.extend([
        format!(
            "{}\n{}",
            agy_command_started(),
            agy_update(&serde_json::json!({"sessionUpdate": "config_option_update",
                "configOptions": agy_options("high", Some("auto"))}))
        ),
        ":".to_owned(),
        format!(
            "{}\n{}\n{}",
            agy_reply(
                5,
                &serde_json::json!({"configOptions": agy_options("high", Some("default"))})
            ),
            agy_command_completed(),
            agy_reply(4, &serde_json::json!({"stopReason": "cancelled"}))
        ),
    ]);
    let (_script, _) = agy_script(&mut c, &steps);
    let (handle, mut events, task) = spawn(c.clone());
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    let seen = agy_run(&handle, &mut events, "hello").await;
    assert_eq!(errors_of(&seen), [agy_ran_unasked("ask")], "{seen:?}");
    assert_eq!(outcome_of(&seen), Outcome::Failed);
    let sent = agy_sent(&c);
    assert_eq!(sent[4]["method"], "session/cancel", "{sent:?}");
    assert_eq!(sent[5]["params"]["configId"], "mode", "{sent:?}");
    assert_eq!(sent[5]["params"]["value"], "default", "{sent:?}");
    agy_scripted_sends_nothing_more(&handle, &mut events, &c).await;
    stop(&handle, task).await;
}
#[tokio::test]
async fn antigravity_the_reason_octet_cancelled_for_is_shown_when_the_cancel_goes_unanswered() {
    // The command completes unasked; Octet cancels, and the vendor never
    // answers the prompt.
    let (_dir, mut c) = antigravity(serde_json::Value::Null);
    let mut steps = agy_handshake();
    steps.extend([
        format!("{}\n{}", agy_command_started(), agy_command_completed()),
        ":".to_owned(),
    ]);
    let (_script, _) = agy_script(&mut c, &steps);
    let limits = Limits {
        interrupt: Duration::from_secs(1),
        ..quick()
    };
    let (handle, mut events, task) = spawn_with_limits(c.clone(), limits);
    wait_for(&mut events, |e| matches!(e, Event::ModeChanged(Mode::Ask))).await;
    handle.send(Command::Prompt("hello".into())).unwrap();
    let error = loop {
        match next(&mut events).await {
            Event::Error(error) => break error,
            Event::Finished { outcome } => panic!("the turn ended {outcome:?} before the error"),
            Event::Stopped => panic!("stopped without an error"),
            _ => {}
        }
    };
    assert_eq!(
        error,
        format!(
            "{}. Antigravity did not stop within 1 second of the cancel; session stopped",
            agy_ran_unasked("ask")
        )
    );
    assert!(matches!(next(&mut events).await, Event::Stopped));
    ended(task).await;
    drop(handle);
}
