use super::*;

#[test]
fn the_goal_marker_shows_neither_on_screen_nor_in_copy() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    for delta in ["DONE ", "[[OCTET_GOAL", "_COMPLETE]]"] {
        app.event(Event::Text(delta.into()));
    }
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Completed,
    });
    assert!(
        !app.entries_text().contains("OCTET_GOAL"),
        "{}",
        app.entries_text()
    );
    assert!(app.entries_text().contains("DONE"));
    assert_eq!(app.last_reply(), Some("DONE "));
    // A held start that was no marker shows when the turn ends.
    app.event(Event::Started);
    app.event(Event::Text("see [[".into()));
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Completed,
    });
    assert!(
        app.entries_text().ends_with("see [["),
        "{}",
        app.entries_text()
    );
}
#[test]
fn a_held_marker_start_shows_before_a_tool_call_or_approval() {
    use octet_core::ToolPhase::Running;
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    // `[` may begin the goal marker, but a tool call settles it: it was the
    // reply's, and belongs before the call.
    app.event(Event::Text("Look at a[".into()));
    app.event(crate::test_support::tool("1", "Read", "a", Running));
    app.event(Event::Text("b]".into()));
    let assistant: Vec<&str> = app
        .chat
        .entries
        .iter()
        .filter(|e| e.role == Role::Assistant)
        .map(|e| e.text.as_str())
        .collect();
    assert_eq!(assistant, ["Look at a[", "b]"]);
    // So does an approval.
    app.event(Event::Text(" then [[OCTET_".into()));
    app.event(Event::Approval {
        id: 1,
        detail: "{}".into(),
        summary: octet_core::ApprovalSummary::default(),
    });
    app.event(Event::Text("done".into()));
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Completed,
    });
    assert_eq!(app.last_reply(), Some("Look at a[\n\nb] then [[OCTET_done"));
}
#[test]
fn copy_keeps_the_last_reply_until_new_text_arrives() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.event(Event::Text("first".into()));
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Completed,
    });
    app.event(Event::Started);
    assert_eq!(app.last_reply(), Some("first"), "the reply on screen");
    app.event(Event::Text("second".into()));
    assert_eq!(app.last_reply(), Some("second"));
}
#[test]
fn copy_takes_the_whole_reply_with_its_tabs() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.event(Event::Text("one".into()));
    app.event(crate::test_support::tool(
        "1",
        "Read",
        "src/main.rs",
        octet_core::ToolPhase::Running,
    ));
    app.event(Event::Text("two\tcolumns".into()));
    assert_eq!(app.last_reply(), Some("one\n\ntwo\tcolumns"));
    app.attach(crate::shell::Ran {
        command: "cat Makefile".into(),
        status: crate::shell::Status::Exited(0),
        output: "all:\n\tcargo build\n".into(),
    });
    assert_eq!(app.composer.attachments[0].output, "all:\n\tcargo build\n");
}
#[tokio::test]
async fn a_new_connection_settles_the_old_sessions_command_and_popup() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.shell = Some(crate::test_support::running_shell(false));
    app.composer.completion = Some(crate::composer::Completion {
        kind: crate::composer::Kind::File,
        items: Vec::new(),
        selected: 0,
        moved: false,
        start: 0,
    });
    let config = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    app.connection(&config, "journal-2".into());
    assert!(!app.shell_running(), "the old session's command is gone");
    assert!(app.composer.completion.is_none());
    assert!(
        app.entries_text()
            .contains("The running command stopped when the session changed"),
        "{}",
        app.entries_text()
    );
}
#[test]
fn a_shell_that_cannot_start_is_an_error_entry() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.error("Cannot run /no/such/shell: No such file or directory");
    assert!(app.last_role() == Some(Role::Error));
    assert!(app.status_line.starts_with("Cannot run"));
}
#[test]
fn shell_output_is_cleaned_and_attachments_stay_bounded() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    let ran = crate::shell::Ran {
        command: "ls -G".into(),
        status: crate::shell::Status::Exited(1),
        output: "\x1b[31mred\x1b[0m\r\nplain\n".into(),
    };
    app.shell_output(&ran);
    let text = app.entries_text();
    assert!(text.contains("$ ls -G\nred\nplain\nexit 1"), "{text:?}");
    assert!(!text.contains('\x1b'));
    app.attach(ran);
    assert!(!app.composer.attachments[0].output.contains('\x1b'));
    let big = crate::shell::Ran {
        command: "big".into(),
        status: crate::shell::Status::Exited(0),
        output: "x".repeat(crate::shell::OUTPUT_LIMIT),
    };
    app.attach(big);
    assert_eq!(
        app.composer.attachments.len(),
        1,
        "the oldest attachment is dropped"
    );
    assert_eq!(app.composer.attachments[0].command, "big");
    assert_eq!(
        app.status_line,
        "Dropped the oldest attachment to stay within 32 KiB"
    );
}
#[test]
fn history_skips_repeats_and_keeps_fifty() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.remember("same".into());
    app.remember("same".into());
    assert_eq!(app.composer.history.len(), 1);
    for i in 0..60 {
        app.remember(format!("p{i}"));
    }
    assert_eq!(app.composer.history.len(), 50);
    assert_eq!(
        app.composer.history.back().map(|sent| sent.text.as_str()),
        Some("p59")
    );
}
#[test]
fn a_full_prompt_warns_instead_of_inserting() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert!(!app.insert_or_warn(&"x".repeat(octet_core::PROMPT_LIMIT + 1)));
    assert_eq!(app.status_line, "Prompt limit reached");
    assert!(app.insert_or_warn("ok"));
}
#[test]
fn a_finished_turn_clears_the_cancelling_notice() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.status(StatusKind::Cancelling, CANCELLING);
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Interrupted,
    });
    assert_eq!(app.status_line, "");
}
#[test]
fn transcript_memory_is_bounded() {
    let c = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    let mut a = App::new(&c, "journal".into());
    for _ in 0..100 {
        a.event(Event::User("x".repeat(64 * 1024)));
        a.event(Event::Text("界".repeat(64 * 1024)));
    }
    assert!(a.chat.bytes <= MAX_BYTES);
    assert!(a.chat.entries.len() <= 160);
    assert!(!a.visible_lines(60, 20).is_empty());
}
#[test]
fn ready_mid_turn_keeps_running() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    assert!(app.is_connecting());
    app.event(Event::Ready {
        session: String::new(),
    });
    assert!(app.is_idle());
    app.event(Event::Started);
    // Claude reports its session ID with the turn's first frames.
    app.event(Event::Ready {
        session: "s-1".into(),
    });
    assert!(app.conn.is_running());
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Completed,
    });
    assert!(app.is_idle());
    app.event(Event::Stopped);
    app.event(Event::Ready {
        session: "late".into(),
    });
    assert!(app.conn.is_stopped() && !app.is_busy() && !app.is_idle());
}
#[test]
fn the_catalog_hint_names_every_vendor() {
    assert_eq!(
        super::catalog_hint(&["codex", "claude", "gemini"]),
        "/model <ID or alias> · /model codex <ID> · /model claude <ID> · /model gemini <ID>"
    );
}
#[test]
fn a_long_streamed_reply_rewraps_in_steps() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    let chunk = format!("{}\n", "streamed words ".repeat(68));
    crate::view::transcript::WRAPPED.set(0);
    for _ in 0..200 {
        app.event(Event::Text(chunk.clone()));
        let _ = app.visible_lines(80, 20);
    }
    let reply = &app.chat.entries.back().unwrap().text;
    assert!(reply.len() <= BLOCK_BYTES + BLOCK_SLACK, "{}", reply.len());
    assert!(reply.ends_with(&chunk));
    // Re-wrapping the whole reply on every chunk would be about 10 MB.
    let wrapped = crate::view::transcript::WRAPPED.get();
    assert!(wrapped < 2 * 1024 * 1024, "re-wrapped {wrapped} bytes");
}

#[test]
fn only_notable_hooks_reach_the_transcript() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    app.event(Event::Hook {
        name: "SessionStart:startup".into(),
        event: "SessionStart".into(),
        outcome: octet_core::HookOutcome::Ran,
        detail: String::new(),
    });
    assert!(!app.entries_text().contains("SessionStart"));
    app.event(Event::Hook {
        name: "PreToolUse:Bash".into(),
        event: "PreToolUse".into(),
        outcome: octet_core::HookOutcome::Decided("allow".into()),
        detail: String::new(),
    });
    assert!(
        app.entries_text()
            .contains("Hook PreToolUse:Bash allowed the tool (Octet was not asked)"),
        "{}",
        app.entries_text()
    );
}

#[test]
fn consecutive_calls_share_one_entry_and_ends_replace_starts() {
    use crate::test_support::tool;
    use octet_core::ToolPhase::{Done, Running};
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.event(tool("1", "Read", "a.rs", Running));
    app.event(tool("2", "Read", "b.rs", Running));
    app.event(tool("1", "Read", "a.rs", Done));
    let entry = app.chat.entries.back().unwrap();
    assert!(entry.role == Role::Tool);
    assert_eq!(
        entry
            .tools
            .iter()
            .map(|c| (c.id.as_str(), c.phase))
            .collect::<Vec<_>>(),
        [("1", Done), ("2", Running)]
    );
    app.event(Event::Text("then".into()));
    app.event(tool("3", "Bash", "ls", Running));
    assert_eq!(
        app.chat
            .entries
            .iter()
            .filter(|e| e.role == Role::Tool)
            .count(),
        2,
        "text starts a new group"
    );
}
#[test]
fn an_unmatched_end_is_its_own_call() {
    use crate::test_support::tool;
    use octet_core::ToolPhase::{Done, Running};
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(tool("9", "Bash", "ls", Done));
    app.event(tool("", "Bash", "a", Running));
    app.event(tool("", "Bash", "a", Done));
    assert_eq!(
        app.chat.entries.back().unwrap().tools.len(),
        3,
        "an empty ID never pairs"
    );
}
#[test]
fn cancelled_calls_say_stopped() {
    use crate::test_support::tool;
    use octet_core::ToolPhase::Running;
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    app.event(tool("1", "Bash", "sleep 9", Running));
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Interrupted,
    });
    let rows = app.visible_lines(80, 10);
    assert!(
        rows.iter()
            .any(|r| r.to_string().contains("Bash sleep 9 · stopped")),
        "{rows:?}"
    );
    assert!(app.running_tool().is_none());
}
#[test]
fn tool_bytes_are_counted_and_trimmed() {
    use crate::test_support::tool;
    use octet_core::ToolPhase::{Done, Running};
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(tool("1", "Read", "a.rs", Running));
    let size = app.chat.entries.back().unwrap().size();
    assert_eq!(app.chat.bytes, size);
    app.event(tool("1", "Read", "a.rs", Done));
    assert_eq!(app.chat.bytes, app.chat.entries.back().unwrap().size());
}
#[test]
fn now_says_what_is_happening() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert_eq!(app.now_line(), "Connecting");
    crate::test_support::idle(&mut app);
    assert_eq!(app.now_line(), "Idle");
    app.event(Event::Started);
    assert_eq!(app.now_line(), "Thinking");
    app.event(Event::Text("x".into()));
    assert_eq!(app.now_line(), "Writing reply");
    app.event(crate::test_support::tool(
        "1",
        "commandExecution",
        "cargo test",
        octet_core::ToolPhase::Running,
    ));
    assert_eq!(app.now_line(), "Running Bash cargo test");
    app.event(crate::test_support::approval(1, "{}"));
    assert_eq!(app.now_line(), "Waiting for approval");
}
#[test]
fn now_ignores_stopped_calls() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    crate::test_support::idle(&mut app);
    app.event(Event::Started);
    app.event(crate::test_support::tool(
        "1",
        "Bash",
        "sleep 9",
        octet_core::ToolPhase::Running,
    ));
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Interrupted,
    });
    assert_eq!(app.now_line(), "Idle");
}
#[test]
fn an_end_finds_its_call_past_a_note() {
    use crate::test_support::tool;
    use octet_core::ToolPhase::{Done, Running};
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    crate::test_support::idle(&mut app);
    app.event(Event::Started);
    app.event(tool("1", "Bash", "ls", Running));
    app.event(Event::Hook {
        name: "PreToolUse:Bash".into(),
        event: "PreToolUse".into(),
        outcome: octet_core::HookOutcome::Decided("allow".into()),
        detail: String::new(),
    });
    app.event(tool("1", "Bash", "ls", Done));
    assert_eq!(app.now_line(), "Thinking");
    app.event(Event::Finished {
        outcome: octet_core::Outcome::Completed,
    });
    let rows: Vec<String> = app
        .visible_lines(80, 20)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        rows.iter()
            .filter(|r| r.contains("▸ Ran 1 command"))
            .count(),
        1,
        "{rows:?}"
    );
    assert!(!rows.iter().any(|r| r.contains("· stopped")), "{rows:?}");
    let sizes: usize = app.chat.entries.iter().map(Entry::size).sum();
    assert_eq!(app.chat.bytes, sizes);
}
#[test]
fn a_turn_ended_without_finishing_stops_its_calls() {
    use crate::test_support::tool;
    use octet_core::ToolPhase::Running;
    let config = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    for stopped in [true, false] {
        let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
        crate::test_support::idle(&mut app);
        app.event(Event::Started);
        app.event(tool("1", "Bash", "cargo test", Running));
        app.event(Event::Error("the vendor exited".into()));
        if stopped {
            app.event(Event::Stopped);
            let rows = app.visible_lines(80, 20);
            assert!(
                rows.iter()
                    .any(|r| r.to_string().contains("Bash cargo test · stopped")),
                "{rows:?}"
            );
        }
        app.connection(&config, "journal-2".into());
        crate::test_support::idle(&mut app);
        app.event(Event::Started);
        assert_eq!(app.now_line(), "Thinking", "stopped: {stopped}");
    }
}
#[test]
fn a_large_tool_run_keeps_its_recent_calls() {
    use octet_core::ToolPhase::{Done, Running};
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::User("read everything".into()));
    app.event(Event::Started);
    app.event(Event::Text("Reading.".into()));
    for n in 1..=17 {
        let id = n.to_string();
        app.event(crate::test_support::tool(&id, "Read", &id, Running));
        app.event(Event::Tool(octet_core::ToolCall {
            id: id.clone(),
            name: "Read".into(),
            target: id,
            phase: Done,
            detail: "x".repeat(32 * 1024),
        }));
    }
    assert!(app.chat.bytes <= MAX_BYTES);
    let ids: Vec<&str> = app
        .chat
        .entries
        .iter()
        .flat_map(|e| e.tools.iter().map(|c| c.id.as_str()))
        .collect();
    // 17 × 32 KiB is past the transcript's limit: only whole old entries go.
    assert_eq!(
        ids,
        (3..=17).map(|n| n.to_string()).collect::<Vec<_>>(),
        "{ids:?}"
    );
    assert!(
        app.chat
            .entries
            .iter()
            .all(|e| e.size() <= BLOCK_BYTES + 33 * 1024)
    );
}
#[test]
fn a_resumed_session_asked_for_no_model_is_not_labelled_the_default() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    let mut config = octet_core::Config::new(octet_core::Engine::CLAUDE, "claude", "/tmp");
    config.resume = Some("s1".into());
    app.connection(&config, "journal-2".into());
    let m = |s: &str, id: &str, name: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(id.into()),
        name: name.into(),
        description: String::new(),
    };
    app.event(Event::Models(vec![
        m("default", "claude-opus-5-5", "Default (recommended)"),
        m("sonnet", "claude-sonnet-5-5", "Sonnet 5.5"),
    ]));
    app.event(Event::Ready {
        session: "s1".into(),
    });
    // Claude resumes on the session's own model, which it names only with
    // the first turn: the header must not claim the account's default.
    assert_eq!(app.conn.model, "the resumed session's model · unconfirmed");
    assert_eq!(app.current_model(), None);
    app.event(Event::ModelSelected("claude-sonnet-5-5".into()));
    assert_eq!(app.conn.model, "Sonnet 5.5 · claude-sonnet-5-5");
    // Asked for by name, the model is the one asked for.
    config.model = Some("sonnet".into());
    app.connection(&config, "journal-3".into());
    app.event(Event::Models(vec![m(
        "sonnet",
        "claude-sonnet-5-5",
        "Sonnet 5.5",
    )]));
    assert_eq!(
        app.conn.model,
        "Sonnet 5.5 · claude-sonnet-5-5 · unconfirmed"
    );
}
