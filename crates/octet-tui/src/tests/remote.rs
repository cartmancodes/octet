//! Commands from a hub, run through the interface's own paths.
use super::*;
use crate::bridge::Remote;
use crate::test_support::{approval, idle};
use octet_core::{
    Mode,
    remote::protocol::{BridgeCommand, CommandName},
};

const CONFIRM: &str = "Full access needs confirm: \"full access\"";

/// Runs `command` as the device "iPhone 15" sent it.
async fn run(
    app: &mut App,
    vendor: &RecordingVendor,
    command: BridgeCommand,
) -> (Result<String, String>, Action) {
    let remote = Remote {
        peer: 1,
        req: 5,
        by: "iPhone 15".into(),
        command,
    };
    crate::bridge::handle_remote(app, vendor, remote, false).await
}

fn prompt(text: &str) -> BridgeCommand {
    BridgeCommand::Prompt { text: text.into() }
}

fn mode(label: &str, confirm: Option<&str>) -> BridgeCommand {
    BridgeCommand::SetMode {
        mode: label.into(),
        confirm: confirm.map(Into::into),
    }
}

#[tokio::test]
async fn a_remote_prompt_sends_or_queues_like_enter() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let (result, action) = run(&mut app, &vendor, prompt("hello")).await;
    assert_eq!(result, Ok("Sent".into()));
    assert!(matches!(action, Action::Continue));
    assert_eq!(vendor.sent.borrow().len(), 1);
    assert!(app.conn.is_running());
    let (result, _) = run(&mut app, &vendor, prompt("next")).await;
    assert_eq!(result, Ok("Queued (1 waiting)".into()));
    assert_eq!(app.composer.queue.len(), 1);
    for _ in 1..crate::app::QUEUE_LIMIT {
        run(&mut app, &vendor, prompt("more")).await;
    }
    let (result, _) = run(&mut app, &vendor, prompt("one too many")).await;
    let Err(reason) = result else {
        panic!("a full queue takes no more")
    };
    assert!(reason.starts_with("The queue is full"), "{reason}");
}

#[tokio::test]
async fn a_slash_prompt_is_refused() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let (result, action) = run(&mut app, &vendor, prompt("/model x")).await;
    assert_eq!(result, Err("Slash commands run on the desk".into()));
    assert!(matches!(action, Action::Continue));
    assert!(vendor.sent.borrow().is_empty());
    let (result, _) = run(&mut app, &vendor, prompt("!ls")).await;
    assert!(result.is_ok(), "a bang is plain text: {result:?}");
}

#[tokio::test]
async fn a_remote_answer_needs_an_open_approval() {
    let mut app = app();
    app.event(approval(1, "first"));
    app.event(approval(2, "second"));
    // The desk's dialog was never drawn; the phone's arm delay has passed.
    assert!(!app.overlay.approval_armed());
    crate::test_support::arm_remote_answers(&mut app);
    let vendor = RecordingVendor::default();
    let answer = |approval, allow| BridgeCommand::Answer { approval, allow };
    let (result, _) = run(&mut app, &vendor, answer(2, true)).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(matches!(
        &vendor.sent.borrow()[..],
        [Command::Answer { id: 2, allow: true }]
    ));
    let ids: Vec<u64> = app.overlay.approvals.iter().map(|asked| asked.id).collect();
    assert_eq!(ids, [1]);
    let (result, _) = run(&mut app, &vendor, answer(9, true)).await;
    assert_eq!(result, Err("No approval 9 is waiting".into()));
    assert_eq!(vendor.sent.borrow().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_remote_answer_waits_for_the_approval_to_arm() {
    const OPENING: &str = "The approval just opened";
    let mut app = app();
    app.event(approval(1, "first"));
    let vendor = RecordingVendor::default();
    let answer = |approval| BridgeCommand::Answer {
        approval,
        allow: true,
    };
    tokio::time::advance(std::time::Duration::from_millis(60)).await;
    let (result, action) = run(&mut app, &vendor, answer(1)).await;
    assert_eq!(result, Err(OPENING.into()));
    assert!(matches!(action, Action::Continue));
    assert!(vendor.sent.borrow().is_empty());
    assert_eq!(app.overlay.approvals.len(), 1);
    assert!(
        !app.entries_text().contains("Allowed from"),
        "{}",
        app.entries_text()
    );
    // Each approval arms on its own: one that arrives later waits its own delay.
    tokio::time::advance(std::time::Duration::from_millis(300)).await;
    app.event(approval(2, "second"));
    tokio::time::advance(std::time::Duration::from_millis(40)).await;
    let (result, _) = run(&mut app, &vendor, answer(1)).await;
    assert!(result.is_ok(), "{result:?}");
    let (result, _) = run(&mut app, &vendor, answer(2)).await;
    assert_eq!(result, Err(OPENING.into()));
    tokio::time::advance(crate::app::APPROVAL_ARM).await;
    let (result, _) = run(&mut app, &vendor, answer(2)).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(matches!(
        &vendor.sent.borrow()[..],
        [
            Command::Answer { id: 1, allow: true },
            Command::Answer { id: 2, allow: true }
        ]
    ));
}

#[tokio::test]
async fn remote_end_keeps_an_unsent_desk_draft() {
    const DRAFT: &str = "The desk has an unsent draft";
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    app.composer.editor.set("a long desk draft".into());
    let (result, action) = run(&mut app, &vendor, BridgeCommand::End).await;
    assert_eq!(result, Err(DRAFT.into()));
    assert!(matches!(action, Action::Continue));
    assert_eq!(app.composer.editor.text(), "a long desk draft");
    app.composer.editor.take();
    app.attach(crate::shell::Ran {
        command: "ls".into(),
        status: crate::shell::Status::Exited(0),
        output: "file\n".into(),
    });
    let (result, action) = run(&mut app, &vendor, BridgeCommand::End).await;
    assert_eq!(result, Err(DRAFT.into()));
    assert!(matches!(action, Action::Continue));
    app.composer.attachments.clear();
    let dir = image_workspace("octet-remote-end-image", &["draft.png"]);
    let image = octet_core::ImageAttachment::open(&dir.path().join("draft.png")).unwrap();
    app.composer.images.push(image);
    let (result, _) = run(&mut app, &vendor, BridgeCommand::End).await;
    assert_eq!(result, Err(DRAFT.into()));
    app.composer.images.clear();
    let (result, action) = run(&mut app, &vendor, BridgeCommand::End).await;
    assert_eq!(result, Ok("Ending".into()));
    assert!(matches!(action, Action::Exit(Exit::Quit)));
}

#[tokio::test]
async fn remote_cancel_mirrors_esc() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let (result, _) = run(&mut app, &vendor, BridgeCommand::Cancel).await;
    assert_eq!(result, Err("No turn is running".into()));
    assert_eq!(vendor.interrupts.get(), 0);
    app.conn.start_turn();
    let (result, _) = run(&mut app, &vendor, BridgeCommand::Cancel).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(vendor.interrupts.get(), 1);
    assert!(app.conn.is_cancelling());
    assert_eq!(app.status_line, "Cancelling…");
}

#[tokio::test]
async fn remote_steer_mirrors_the_steer_command() {
    let steer = |text: &str| BridgeCommand::Steer { text: text.into() };
    let mut app = app();
    app.conn.start_turn();
    let vendor = RecordingVendor::default();
    let (result, _) = run(&mut app, &vendor, steer("look at docs")).await;
    assert_eq!(result, Ok("Steering".into()));
    assert!(matches!(
        &vendor.sent.borrow()[..],
        [Command::Steer(text)] if text == "look at docs"
    ));
    // Idle, a steer goes as a prompt, as `/steer` does at the desk.
    idle(&mut app);
    let (result, _) = run(&mut app, &vendor, steer("start here")).await;
    assert_eq!(result, Ok("Sent as a prompt".into()));
    assert!(matches!(
        vendor.sent.borrow().last(),
        Some(Command::Prompt(text)) if text == "start here"
    ));
    let config = Config::new(Engine::CLAUDE, "claude", "/tmp");
    let mut app = App::new(&config, "journal".into());
    app.conn.start_turn();
    let vendor = RecordingVendor::default();
    let (result, _) = run(&mut app, &vendor, steer("look at docs")).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(app.composer.queue.len(), 1);
    assert!(
        app.entries_text().contains("Claude cannot steer"),
        "{}",
        app.entries_text()
    );
}

#[tokio::test]
async fn remote_mode_changes_follow_the_mode_command() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let (result, action) = run(&mut app, &vendor, mode("auto", None)).await;
    assert_eq!(result, Ok("Switching to auto".into()));
    assert!(matches!(action, Action::Continue));
    assert!(matches!(
        &vendor.sent.borrow()[..],
        [Command::SetMode(Mode::Auto)]
    ));
    let (result, _) = run(&mut app, &vendor, mode("sideways", None)).await;
    assert!(result.is_err());
    let mut app = super::app();
    idle(&mut app);
    let (result, action) = run(&mut app, &vendor, mode("full-access", None)).await;
    assert_eq!(result, Err(CONFIRM.into()));
    assert!(matches!(action, Action::Continue));
    let (result, _) = run(&mut app, &vendor, mode("full-access", Some("yes"))).await;
    assert_eq!(result, Err(CONFIRM.into()));
    let (result, action) = run(&mut app, &vendor, mode("full-access", Some("full access"))).await;
    assert_eq!(result, Ok("Reconnecting in full-access".into()));
    assert!(matches!(action, Action::Exit(Exit::Mode(Mode::FullAccess))));
    app.conn.start_turn();
    let (result, action) = run(&mut app, &vendor, mode("full-access", Some("full access"))).await;
    assert_eq!(result, Err(TURN_OPEN.into()));
    assert!(matches!(action, Action::Continue));
}

#[tokio::test]
async fn remote_model_changes_follow_the_model_command() {
    let select = |selection: &str| BridgeCommand::SetModel {
        selection: selection.into(),
    };
    let listed = octet_core::ModelInfo {
        selection: "opus".into(),
        id: Some("opus".into()),
        name: "opus".into(),
        description: String::new(),
    };
    let mut app = app();
    idle(&mut app);
    app.models
        .catalogs
        .set(Engine::CLAUDE, vec![listed], std::time::SystemTime::now());
    let vendor = RecordingVendor::default();
    for bad in [
        "",
        "two words",
        "tab\tbed",
        "line\nbreak",
        "list",
        "refresh",
    ] {
        let (result, action) = run(&mut app, &vendor, select(bad)).await;
        assert!(result.is_err(), "{bad:?}: {result:?}");
        assert!(matches!(action, Action::Continue));
    }
    app.conn.start_turn();
    let (result, action) = run(&mut app, &vendor, select("opus")).await;
    assert_eq!(result, Err(TURN_OPEN.into()));
    assert!(matches!(action, Action::Continue));
    idle(&mut app);
    let (result, action) = run(&mut app, &vendor, select("opus")).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(matches!(
        action,
        Action::Exit(Exit::Model(selection)) if selection.provider == Engine::CLAUDE
    ));
}

#[tokio::test]
async fn remote_end_refuses_during_a_turn() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let (result, action) = run(&mut app, &vendor, BridgeCommand::End).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(matches!(action, Action::Exit(Exit::Quit)));
    app.conn.start_turn();
    let (result, action) = run(&mut app, &vendor, BridgeCommand::End).await;
    assert_eq!(result, Err(TURN_OPEN.into()));
    assert!(matches!(action, Action::Continue));
}

#[tokio::test]
async fn remote_actions_are_noted_on_the_desk() {
    let mut app = app();
    app.event(approval(1, "first"));
    crate::test_support::arm_remote_answers(&mut app);
    let vendor = RecordingVendor::default();
    let remote = |by: &str, command| Remote {
        peer: 1,
        req: 1,
        by: by.into(),
        command,
    };
    crate::bridge::handle_remote(
        &mut app,
        &vendor,
        remote(
            "Gemini 10",
            BridgeCommand::Answer {
                approval: 1,
                allow: true,
            },
        ),
        false,
    )
    .await;
    assert!(
        app.entries_text().contains("Allowed from Gemini 10"),
        "{}",
        app.entries_text()
    );
    idle(&mut app);
    crate::bridge::handle_remote(&mut app, &vendor, remote("Gemini 10", prompt("hi")), false).await;
    assert!(
        app.entries_text().contains("Prompt from Gemini 10"),
        "{}",
        app.entries_text()
    );
    crate::bridge::handle_remote(
        &mut app,
        &vendor,
        remote("a\u{1b}[31mb", BridgeCommand::Cancel),
        false,
    )
    .await;
    let text = app.entries_text();
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("Cancelled from "), "{text}");
}

#[tokio::test]
async fn a_remote_prompt_leaves_the_desks_attachments_and_history_alone() {
    let dir = image_workspace("octet-remote-image", &["draft.png"]);
    let mut app = app();
    idle(&mut app);
    app.remember("earlier".into());
    app.attach(crate::shell::Ran {
        command: "git status".into(),
        status: crate::shell::Status::Exited(0),
        output: "clean\n".into(),
    });
    let image = octet_core::ImageAttachment::open(&dir.path().join("draft.png")).unwrap();
    app.composer.images.push(image);
    let vendor = RecordingVendor::default();
    let (result, _) = run(&mut app, &vendor, prompt("from the phone")).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(matches!(
        &vendor.sent.borrow()[..],
        [Command::Prompt(text)] if text == "from the phone"
    ));
    assert_eq!(app.composer.attachments.len(), 1);
    assert_eq!(app.composer.images.len(), 1);
    assert_eq!(app.composer.history.len(), 1);
    // A steer that falls back to a prompt does the same.
    let config = Config::new(Engine::CLAUDE, "claude", "/tmp");
    let mut claude = App::new(&config, "journal".into());
    claude.attach(crate::shell::Ran {
        command: "git status".into(),
        status: crate::shell::Status::Exited(0),
        output: "clean\n".into(),
    });
    claude.conn.start_turn();
    let steer = BridgeCommand::Steer {
        text: "look".into(),
    };
    let (result, _) = run(&mut claude, &vendor, steer).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(claude.composer.queue.len(), 1);
    assert_eq!(claude.composer.attachments.len(), 1);
    assert!(claude.composer.history.is_empty());
}

#[tokio::test]
async fn slash_text_is_refused_for_prompts_and_steers() {
    let mut app = app();
    app.conn.start_turn();
    let vendor = RecordingVendor::default();
    for text in ["/model x", "  \t/model x", "\n/quit"] {
        let steer = BridgeCommand::Steer { text: text.into() };
        for command in [prompt(text), steer] {
            let (result, _) = run(&mut app, &vendor, command).await;
            assert_eq!(
                result,
                Err("Slash commands run on the desk".into()),
                "{text:?}"
            );
        }
    }
    assert!(vendor.sent.borrow().is_empty());
}

#[tokio::test]
async fn a_refused_remote_command_names_its_device_on_the_desk() {
    const FROM: &str = "Refused from iPhone 15: ";
    let vendor = RecordingVendor::default();
    // Whatever a refusal adds to the transcript or the status line names the device.
    let added = |app: &App, before: usize| -> Vec<String> {
        app.chat
            .entries
            .iter()
            .skip(before)
            .map(|entry| entry.text.clone())
            .collect()
    };
    // A mode change while one is pending: the mode command notes why.
    let mut app = app();
    idle(&mut app);
    app.conn.mode_pending = Some(Mode::Auto);
    let before = app.chat.entries.len();
    let (result, _) = run(&mut app, &vendor, mode("accept-edits", None)).await;
    let reason = "Mode change pending; wait for the vendor to confirm";
    assert_eq!(result, Err(reason.into()));
    assert_eq!(added(&app, before), [format!("{FROM}{reason}")]);
    assert_eq!(app.status_line, format!("{FROM}{reason}"));
    // A model change during a turn: the model command hints why.
    let mut app = super::app();
    app.models.catalogs.set(
        Engine::CLAUDE,
        vec![octet_core::ModelInfo {
            selection: "opus".into(),
            id: Some("opus".into()),
            name: "opus".into(),
            description: String::new(),
        }],
        std::time::SystemTime::now(),
    );
    app.conn.start_turn();
    let before = app.chat.entries.len();
    let select = BridgeCommand::SetModel {
        selection: "opus".into(),
    };
    let (result, _) = run(&mut app, &vendor, select).await;
    assert_eq!(result, Err(TURN_OPEN.into()));
    assert!(added(&app, before).is_empty());
    assert_eq!(app.status_line, format!("{FROM}{TURN_OPEN}"));
    // A prompt into a full queue.
    for _ in 0..crate::app::QUEUE_LIMIT {
        run(&mut app, &vendor, prompt("more")).await;
    }
    let (result, _) = run(&mut app, &vendor, prompt("one too many")).await;
    let reason = result.expect_err("the queue is full");
    assert_eq!(app.status_line, format!("{FROM}{reason}"));
    // A refusal the desk was never told of stays off it.
    let mut app = super::app();
    idle(&mut app);
    app.hint("desk status");
    let before = app.chat.entries.len();
    let (result, _) = run(&mut app, &vendor, BridgeCommand::Cancel).await;
    assert_eq!(result, Err("No turn is running".into()));
    assert!(added(&app, before).is_empty());
    assert_eq!(app.status_line, "desk status");
}

#[tokio::test]
async fn a_path_like_prompt_is_sent_as_on_the_desk() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    // The desk sends these as prompts: no command name contains a slash, and the demo is text.
    for text in ["/usr/lib is broken, fix it", octet_core::APPROVAL_DEMO] {
        idle(&mut app);
        let (result, _) = run(&mut app, &vendor, prompt(text)).await;
        assert!(result.is_ok(), "{text:?}: {result:?}");
        assert!(
            matches!(vendor.sent.borrow().last(), Some(Command::Prompt(sent)) if sent == text),
            "{text:?}"
        );
    }
    let steer = BridgeCommand::Steer {
        text: "/usr/lib is broken".into(),
    };
    let (result, _) = run(&mut app, &vendor, steer).await;
    assert!(result.is_ok(), "{result:?}");
    // What the desk runs as a command is still refused.
    for text in [
        "/model x",
        "/model",
        "/skills",
        " /quit\n",
        "/unknown thing",
    ] {
        let (result, _) = run(&mut app, &vendor, prompt(text)).await;
        assert_eq!(
            result,
            Err("Slash commands run on the desk".into()),
            "{text:?}"
        );
    }
}

#[tokio::test]
async fn a_claude_steer_queues_with_the_words_the_desk_uses() {
    const QUEUED: &str = "Claude cannot steer a running turn; queued as a follow-up";
    let config = Config::new(Engine::CLAUDE, "claude", "/tmp");
    let steer = BridgeCommand::Steer {
        text: "look at docs".into(),
    };
    let vendor = RecordingVendor::default();
    let mut remote = App::new(&config, "journal".into());
    remote.conn.start_turn();
    let (result, action) = run(&mut remote, &vendor, steer).await;
    assert_eq!(result, Ok(QUEUED.into()));
    assert!(matches!(action, Action::Continue));
    // The desk's `/steer` does the same: it queues, and says so in the same words.
    let mut desk = App::new(&config, "journal".into());
    desk.conn.start_turn();
    commands::command(&mut desk, &vendor, "/steer look at docs").await;
    assert_eq!(desk.status_line, QUEUED);
    assert_eq!(desk.composer.queue.len(), remote.composer.queue.len());
    assert!(vendor.sent.borrow().is_empty(), "nothing went to Claude");
}

#[tokio::test]
async fn device_names_are_one_short_line_in_desk_notes() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let remote = |by: &str| Remote {
        peer: 1,
        req: 1,
        by: by.into(),
        command: BridgeCommand::Cancel,
    };
    app.conn.start_turn();
    crate::bridge::handle_remote(&mut app, &vendor, remote("My\niPhone \n\n  15"), false).await;
    assert!(
        app.entries_text().contains("Cancelled from My iPhone 15"),
        "{}",
        app.entries_text()
    );
    let mut app = super::app();
    app.conn.start_turn();
    crate::bridge::handle_remote(&mut app, &vendor, remote(&"é".repeat(500)), false).await;
    let text = app.entries_text();
    let name = text.strip_prefix("Cancelled from ").expect(&text);
    assert_eq!(name.chars().count(), 40, "{name}");
    assert!(name.ends_with('…'));
}

#[tokio::test]
async fn full_access_from_a_phone_leaves_a_warning_note() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let (result, _) = run(&mut app, &vendor, mode("full-access", Some("full access"))).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        app.entries_text().contains("⚠ Full access from iPhone 15"),
        "{}",
        app.entries_text()
    );
}

/// Runs `command` from "iPhone 15" while the desk's external editor has the terminal.
async fn run_editing(
    app: &mut App,
    vendor: &RecordingVendor,
    command: BridgeCommand,
) -> (Result<String, String>, Action) {
    let remote = Remote {
        peer: 1,
        req: 5,
        by: "iPhone 15".into(),
        command,
    };
    crate::bridge::handle_remote(app, vendor, remote, true).await
}

#[tokio::test]
async fn the_desks_editor_or_bang_command_is_never_ended_from_a_phone() {
    const EDITING: &str = "The desk is editing a prompt";
    const SHELL: &str = "A ! command is running at the desk";
    let listed = octet_core::ModelInfo {
        selection: "opus".into(),
        id: Some("opus".into()),
        name: "opus".into(),
        description: String::new(),
    };
    let select = || BridgeCommand::SetModel {
        selection: "opus".into(),
    };
    let vendor = RecordingVendor::default();
    let mut app = app();
    idle(&mut app);
    app.models
        .catalogs
        .set(Engine::CLAUDE, vec![listed], std::time::SystemTime::now());
    for (command, wanted) in [
        (BridgeCommand::End, EDITING),
        (select(), EDITING),
        (mode("full-access", Some("full access")), EDITING),
    ] {
        let (result, action) = run_editing(&mut app, &vendor, command).await;
        assert_eq!(result, Err(wanted.into()));
        assert!(matches!(action, Action::Continue));
    }
    app.shell = Some(crate::test_support::running_shell(false));
    for command in [
        BridgeCommand::End,
        select(),
        mode("full-access", Some("full access")),
    ] {
        let (result, action) = run(&mut app, &vendor, command).await;
        assert_eq!(result, Err(SHELL.into()));
        assert!(matches!(action, Action::Continue));
    }
    // What cannot end the session still works: a prompt, and a mode change without a reconnect.
    let (result, _) = run_editing(&mut app, &vendor, prompt("from the phone")).await;
    assert!(result.is_ok(), "{result:?}");
    idle(&mut app);
    let (result, action) = run(&mut app, &vendor, mode("auto", None)).await;
    assert_eq!(result, Ok("Switching to auto".into()));
    assert!(matches!(action, Action::Continue));
    assert!(app.shell.is_some(), "the ! command runs on");
}

#[tokio::test]
async fn a_remote_model_may_not_look_like_an_option() {
    let mut app = app();
    idle(&mut app);
    let vendor = RecordingVendor::default();
    let select = BridgeCommand::SetModel {
        selection: "-x".into(),
    };
    let (result, action) = run(&mut app, &vendor, select).await;
    assert_eq!(result, Err("Name one model, without spaces".into()));
    assert!(matches!(action, Action::Continue));
}

fn typed(name: CommandName) -> BridgeCommand {
    BridgeCommand::Command { name }
}

#[tokio::test]
async fn new_is_refused_like_the_desk() {
    const EDITING: &str = "The desk is editing a prompt";
    const SHELL: &str = "A ! command is running at the desk";
    const DRAFT: &str = "The desk has an unsent draft";
    let vendor = RecordingVendor::default();
    let mut app = app();
    idle(&mut app);
    // The external editor.
    let (result, action) = run_editing(&mut app, &vendor, typed(CommandName::New)).await;
    assert_eq!(result, Err(EDITING.into()));
    assert!(matches!(action, Action::Continue));
    // A `!` command.
    app.shell = Some(crate::test_support::running_shell(false));
    let (result, action) = run(&mut app, &vendor, typed(CommandName::New)).await;
    assert_eq!(result, Err(SHELL.into()));
    assert!(matches!(action, Action::Continue));
    app.shell = None;
    // A draft: typed text, `!` output, or images.
    app.composer.editor.set("a long desk draft".into());
    let (result, action) = run(&mut app, &vendor, typed(CommandName::New)).await;
    assert_eq!(result, Err(DRAFT.into()));
    assert!(matches!(action, Action::Continue));
    assert_eq!(app.composer.editor.text(), "a long desk draft");
    app.composer.editor.take();
    app.attach(crate::shell::Ran {
        command: "ls".into(),
        status: crate::shell::Status::Exited(0),
        output: "file\n".into(),
    });
    let (result, action) = run(&mut app, &vendor, typed(CommandName::New)).await;
    assert_eq!(result, Err(DRAFT.into()));
    assert!(matches!(action, Action::Continue));
    app.composer.attachments.clear();
    // A turn.
    app.conn.start_turn();
    let (result, action) = run(&mut app, &vendor, typed(CommandName::New)).await;
    assert_eq!(result, Err(TURN_OPEN.into()));
    assert!(matches!(action, Action::Continue));
    // Idle, it starts afresh, as `/new` does at the desk.
    let mut app = self::app();
    idle(&mut app);
    let (result, action) = run(&mut app, &vendor, typed(CommandName::New)).await;
    assert_eq!(result, Ok("Starting a fresh conversation".into()));
    assert!(matches!(action, Action::Exit(Exit::New)));
    assert!(
        vendor.sent.take().is_empty(),
        "nothing was sent to the vendor"
    );
}

#[tokio::test]
async fn the_session_text_is_the_desks_own() {
    let vendor = RecordingVendor::default();
    let mut remote = app();
    idle(&mut remote);
    let (result, action) = run(&mut remote, &vendor, typed(CommandName::Session)).await;
    let reply = result.expect("the text");
    assert!(matches!(action, Action::Continue));
    assert_eq!(reply, commands::session_text(&remote));
    assert!(reply.contains("Session: thread-1"), "{reply}");
    assert!(reply.contains("Journal: journal"), "{reply}");
    assert!(reply.contains("Workspace: "), "{reply}");
    // The desk's `/session` puts those words in the transcript.
    let mut desk = app();
    idle(&mut desk);
    command(&mut desk, "/session").await;
    assert!(
        desk.entries_text().contains(&reply),
        "{}",
        desk.entries_text()
    );
}

#[tokio::test]
async fn the_help_text_names_what_the_phone_can_run() {
    let vendor = RecordingVendor::default();
    let mut app = app();
    idle(&mut app);
    let (result, action) = run(&mut app, &vendor, typed(CommandName::Help)).await;
    assert_eq!(
        result,
        Ok(
            "Commands from the phone: /model · /mode · /new · /session · /export · /help. \
            Everything else runs at the desk."
                .into()
        )
    );
    assert!(matches!(action, Action::Continue));
}

#[tokio::test]
async fn export_answers_with_the_desk_words() {
    let workspace = octet_testkit::TempDir::new("octet-remote-export");
    let data = octet_testkit::TempDir::new("octet-remote-export-data");
    std::fs::create_dir_all(workspace.path()).unwrap();
    std::fs::create_dir_all(data.path()).unwrap();
    let journal = data.path().join("session-7.jsonl");
    std::fs::write(&journal, b"{}\n").unwrap();
    let vendor = RecordingVendor::default();
    let mut app = app();
    idle(&mut app);
    app.composer.root = workspace.path().to_path_buf();
    app.conn.journal = journal;
    // Always the desk's default: the workspace and the journal's file name.
    let target = workspace.path().join("session-7.jsonl");
    let (result, action) = run(&mut app, &vendor, typed(CommandName::Export)).await;
    assert_eq!(
        result,
        Ok(format!("Exporting the journal to {}", target.display()))
    );
    assert!(matches!(&action, Action::Job(job) if job.label == "Exporting the journal…"));
    // The job is the desk's own, and its outcome reaches the desk as it does for `/export`.
    run_job(&mut app, action).await;
    assert!(
        app.entries_text()
            .contains(&format!("Exported journal to {}", target.display())),
        "{}",
        app.entries_text()
    );
    let before = std::fs::read(&target).unwrap();
    // A file of that name is already there: the desk's export refuses to overwrite, in its words,
    // and the phone's command still only starts the job.
    let (result, action) = run(&mut app, &vendor, typed(CommandName::Export)).await;
    assert!(result.is_ok(), "{result:?}");
    run_job(&mut app, action).await;
    assert!(
        app.entries_text().contains("Export failed: Cannot create"),
        "{}",
        app.entries_text()
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "the first export is kept"
    );
}

#[tokio::test]
async fn export_failing_or_waiting_says_what_the_desk_says() {
    let vendor = RecordingVendor::default();
    // A workspace that is not there: the desk's export job reports its own error.
    let mut app = app();
    idle(&mut app);
    app.composer.root = "/nonexistent-octet-workspace/inner".into();
    let (result, action) = run(&mut app, &vendor, typed(CommandName::Export)).await;
    assert!(result.is_ok(), "{result:?}");
    run_job(&mut app, action).await;
    assert!(
        app.entries_text().contains("Export failed"),
        "{}",
        app.entries_text()
    );
    // A job already running: the desk's `Wait for` words, and no second job.
    let mut busy = self::app();
    idle(&mut busy);
    busy.job = Some(crate::jobs::Running::spawn(crate::jobs::Job {
        label: "Reading recent sessions…",
        must_finish: false,
        work: Box::pin(std::future::pending()),
    }));
    let (result, action) = run(&mut busy, &vendor, typed(CommandName::Export)).await;
    assert_eq!(result, Err("Wait for: Reading recent sessions…".into()));
    assert!(matches!(action, Action::Continue));
}

#[tokio::test]
async fn each_typed_command_leaves_a_desk_note_naming_the_device() {
    let vendor = RecordingVendor::default();
    for (name, wanted) in [
        (CommandName::New, "New conversation from iPhone 15"),
        (CommandName::Session, "Session from iPhone 15"),
        (CommandName::Help, "Help from iPhone 15"),
        (CommandName::Export, "Export from iPhone 15"),
    ] {
        let mut app = app();
        idle(&mut app);
        let (result, _) = run(&mut app, &vendor, typed(name)).await;
        assert!(result.is_ok(), "{name:?}: {result:?}");
        assert!(
            app.entries_text().contains(wanted),
            "{name:?}: {}",
            app.entries_text()
        );
    }
}
