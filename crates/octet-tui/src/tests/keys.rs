//! Keys, paste and prompt submission.
use super::*;

#[tokio::test]
async fn at_opens_the_file_popup_and_enter_accepts() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.composer.files = crate::files::Files::Ready(crate::files::Index::from_paths(vec![
        "README.md".into(),
        "src/main.rs".into(),
    ]));
    for c in "see @mai".chars() {
        key_action(&mut app, &vendor, key(KeyCode::Char(c))).await;
    }
    let completion = app.composer.completion.as_ref().expect("popup open");
    assert_eq!(completion.items, ["src/main.rs"]);
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "see @src/main.rs ");
    assert!(app.composer.completion.is_none());
    assert!(!app.conn.is_running(), "Enter accepted instead of sending");
}
#[tokio::test]
async fn the_palette_offers_the_editor_mentions_and_shell() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    let open_at = |app: &mut App, index: usize| {
        app.overlay.palette = true;
        app.overlay.selection = index;
    };
    open_at(&mut app, COMMANDS.len() + 1);
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "@");
    assert!(app.composer.completion.is_some(), "@ opens the file popup");
    app.composer.completion = None;
    app.composer.editor.take();
    open_at(&mut app, COMMANDS.len() + 2);
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "!");
    open_at(&mut app, COMMANDS.len());
    assert!(matches!(
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await,
        Action::ExternalEditor
    ));
    app.overlay.palette = true;
    app.overlay.selection = 0;
    for _ in 0..40 {
        key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    }
    assert_eq!(
        app.overlay.selection,
        COMMANDS.len() + view::PALETTE_KEYS.len() - 1
    );
}
#[tokio::test]
async fn a_prompt_too_long_for_its_attachments_says_so() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.attach(ran("echo hi", "hi\n"));
    app.composer
        .editor
        .set("x".repeat(octet_core::PROMPT_LIMIT - 6));
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(
        app.status_line,
        "The prompt and its attachments are over 64 KiB. Shorten the prompt, or press Esc on an empty prompt to drop them"
    );
    assert_eq!(app.composer.attachments.len(), 1, "kept");
    assert!(!app.conn.is_running());
}
#[tokio::test]
async fn the_popup_closes_before_the_editor_and_follows_a_paste() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.composer.files =
        crate::files::Files::Ready(crate::files::Index::from_paths(vec!["src/main.rs".into()]));
    for c in "a long draft @ma".chars() {
        key_action(&mut app, &vendor, key(KeyCode::Char(c))).await;
    }
    assert!(app.composer.completion.is_some());
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('g')).await,
        Action::ExternalEditor
    ));
    assert!(
        app.composer.completion.is_none(),
        "the edited draft must not meet a stale popup"
    );
    app.composer.editor.set("/re".into());
    app.composer.completion = Some(composer::Completion {
        kind: composer::Kind::Command,
        items: vec!["/reconnect".into()],
        selected: 0,
        start: 0,
    });
    paste(&mut app, "port the bug");
    assert!(
        app.composer.completion.is_none(),
        "a paste closes a command popup"
    );
    key_action(&mut app, &vendor, key(KeyCode::Char('x'))).await;
    assert_eq!(app.composer.editor.text(), "/report the bugx");
}
#[tokio::test]
async fn the_first_at_asks_for_the_index_and_esc_closes_the_popup() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    key_action(&mut app, &vendor, key(KeyCode::Char('@'))).await;
    assert!(matches!(app.composer.files, crate::files::Files::Wanted));
    assert!(app.composer.completion.is_some());
    key_action(&mut app, &vendor, key(KeyCode::Esc)).await;
    assert!(app.composer.completion.is_none());
    assert_eq!(app.composer.editor.text(), "@");
}
#[tokio::test]
async fn an_at_inside_a_word_opens_nothing() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    for c in "me@host".chars() {
        key_action(&mut app, &vendor, key(KeyCode::Char(c))).await;
    }
    assert!(app.composer.completion.is_none());
}
#[tokio::test]
async fn bang_lines_run_locally_and_double_bang_does_not_attach() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.composer.editor.set("!echo hi".into());
    assert!(matches!(
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await,
        Action::RunShell { ref command, attach: true } if command == "echo hi"
    ));
    assert!(app.composer.editor.text().is_empty());
    assert_eq!(
        app.composer.history.back().map(|sent| sent.text.as_str()),
        Some("!echo hi")
    );
    app.composer.editor.set("!!  pwd ".into());
    assert!(matches!(
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await,
        Action::RunShell { ref command, attach: false } if command == "pwd"
    ));
    app.composer.editor.set("!".into());
    assert!(matches!(
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await,
        Action::Continue
    ));
    assert_eq!(app.status_line, "Type a command after !");
    app.shell = Some(crate::test_support::running_shell(false));
    app.composer.editor.set("!ls".into());
    assert!(matches!(
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await,
        Action::Continue
    ));
    assert_eq!(app.status_line, "A command is already running");
    assert!(matches!(
        key_action(&mut app, &vendor, key(KeyCode::Esc)).await,
        Action::CancelShell
    ));
}
#[tokio::test]
async fn attachments_go_with_the_next_prompt_then_clear() {
    let mut app = app();
    let (_temp, mut session) = demo_session("octet-attach").await;
    app.attach(ran("echo hi", "hi\n"));
    app.composer.editor.set("explain".into());
    key_action(&mut app, &session.handle, key(KeyCode::Enter)).await;
    assert!(app.composer.attachments.is_empty());
    assert_eq!(next_user_text(&mut session).await, "explain\n\n[+ echo hi]");
}
#[tokio::test]
async fn esc_on_an_empty_idle_draft_removes_attachments() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.attach(ran("ls", ""));
    key_action(&mut app, &vendor, key(KeyCode::Esc)).await;
    assert!(app.composer.attachments.is_empty());
    assert_eq!(app.status_line, "Attachments removed");
}
#[tokio::test]
async fn ctrl_c_twice_on_an_idle_empty_prompt_quits() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    assert_eq!(app.status_line, QUIT_HINT);
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Exit(Exit::Quit)
    ));
}
#[tokio::test]
async fn ctrl_c_clears_a_draft_before_it_can_quit() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    assert!(app.composer.editor.insert("half-written prompt"));
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    assert!(app.composer.editor.text().is_empty());
    assert_ne!(app.status_line, QUIT_HINT);
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Exit(Exit::Quit)
    ));
}
#[tokio::test]
async fn another_key_or_an_expired_window_disarms_quit() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    key_action(&mut app, &vendor, ctrl('c')).await;
    let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
    key_action(&mut app, &vendor, left).await;
    assert_ne!(app.status_line, QUIT_HINT, "another key clears the hint");
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    // The window has passed: the press arms again instead of quitting.
    app.quit_armed = Some(tokio::time::Instant::now() - Duration::from_millis(1));
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    assert_eq!(app.status_line, QUIT_HINT);
}
#[tokio::test]
async fn ctrl_c_closes_help_and_the_palette_without_quitting() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.overlay.help = true;
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    assert!(!app.overlay.help);
    app.overlay.palette = true;
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('c')).await,
        Action::Continue
    ));
    assert!(!app.overlay.palette);
    assert_ne!(app.status_line, QUIT_HINT);
}
#[tokio::test]
async fn ctrl_c_interrupts_a_running_turn_instead_of_quitting() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.conn.start_turn();
    for _ in 0..2 {
        assert!(matches!(
            key_action(&mut app, &vendor, ctrl('c')).await,
            Action::Continue
        ));
        assert_eq!(app.status_line, crate::app::CANCELLING);
    }
}
#[tokio::test]
async fn ctrl_q_no_longer_quits() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    assert!(matches!(
        key_action(&mut app, &vendor, ctrl('q')).await,
        Action::Continue
    ));
}
#[tokio::test]
async fn ctrl_c_in_approval_dialog_pauses_the_active_goal() {
    let mut app = app();
    app.goals.set_goal(Some(
        octet_core::goal::Goal::new("Ship the project").unwrap(),
    ));
    app.goals.goal_prompt_sent();
    app.conn.start_turn();
    app.overlay
        .approvals
        .push_back(crate::test_support::asked(1, "command"));
    let temp = octet_testkit::TempDir::new("octet-goal-cancel");
    let directory = temp.path().to_path_buf();
    let config = Config::new(Engine::DEMO, "demo", directory.clone());
    let session = Session::open(config, directory.clone()).await.unwrap();
    key_action(
        &mut app,
        &session.handle,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    )
    .await;
    session.shutdown().await;
    assert_eq!(
        app.goals.goal().unwrap().status(),
        octet_core::goal::Status::Paused
    );
}
#[test]
fn cycle_follows_order_and_never_reaches_full_access() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    cycle_mode(&mut app, &vendor);
    app.conn.mode_pending = None;
    app.conn.mode = octet_core::Mode::Auto;
    cycle_mode(&mut app, &vendor);
    app.conn.mode_pending = None;
    app.conn.mode = octet_core::Mode::FullAccess;
    cycle_mode(&mut app, &vendor);
    assert!(app.status_line.contains("/mode"));
    assert_eq!(
        vendor.sent.take(),
        [
            Command::SetMode(octet_core::Mode::AcceptEdits),
            Command::SetMode(octet_core::Mode::Ask)
        ]
    );
}
#[tokio::test]
async fn unknown_command_keeps_the_draft() {
    let temp = octet_testkit::TempDir::new("octet-tui-draft");
    let directory = temp.path().to_path_buf();
    let config = Config::new(Engine::DEMO, "demo", directory.clone());
    let session = Session::open(config.clone(), directory.clone())
        .await
        .unwrap();
    let mut app = App::new(&config, session.journal().to_path_buf());
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.composer
            .editor
            .insert("/nope is not a command, keep my words")
    );
    assert!(matches!(
        key_action(&mut app, &session.handle, enter).await,
        Action::Continue
    ));
    assert_eq!(
        app.composer.editor.text(),
        "/nope is not a command, keep my words"
    );
    assert!(app.status_line.contains("Unknown command"));
    app.composer.editor.take();
    // A path is a prompt, not a command.
    crate::test_support::idle(&mut app);
    assert!(
        app.composer
            .editor
            .insert("/usr/lib is where this breaks, please look")
    );
    assert!(matches!(
        key_action(&mut app, &session.handle, enter).await,
        Action::Continue
    ));
    assert!(
        app.composer.editor.text().is_empty() && app.conn.is_running(),
        "{}",
        app.status_line
    );
    assert_eq!(
        app.composer.history.back().map(|sent| sent.text.as_str()),
        Some("/usr/lib is where this breaks, please look")
    );
    crate::test_support::idle(&mut app);
    // A prompt starting with a multi-byte character is an ordinary prompt.
    assert!(app.composer.editor.insert("界 means world"));
    assert!(matches!(
        key_action(&mut app, &session.handle, enter).await,
        Action::Continue
    ));
    assert_eq!(
        app.composer.history.back().map(|sent| sent.text.as_str()),
        Some("界 means world")
    );
    crate::test_support::idle(&mut app);
    assert!(app.composer.editor.insert("/session"));
    assert!(matches!(
        key_action(&mut app, &session.handle, enter).await,
        Action::Continue
    ));
    assert!(app.composer.editor.text().is_empty());
    session.shutdown().await;
}
#[tokio::test]
async fn enter_while_running_queues_the_prompt() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.conn.start_turn();
    assert!(app.composer.editor.insert("next please"));
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.queue.len(), 1);
    assert!(app.composer.editor.text().is_empty());
    assert_eq!(crate::view::chips(&app).as_deref(), Some("1 queued"));
}
#[tokio::test]
async fn cancel_drops_the_queue() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.conn.start_turn();
    app.composer.queue.push_back(Prompt::plain("a".into()));
    app.composer.queue.push_back(Prompt::plain("b".into()));
    key_action(&mut app, &vendor, key(KeyCode::Esc)).await;
    assert!(app.composer.queue.is_empty());
    assert!(app.entries_text().contains("Dropped 2 queued prompts"));
}
#[tokio::test]
async fn the_queue_is_bounded() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.conn.start_turn();
    for i in 0..8 {
        app.composer.queue.push_back(Prompt::plain(format!("p{i}")));
    }
    assert!(app.composer.editor.insert("one more"));
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.queue.len(), 8);
    assert_eq!(app.composer.editor.text(), "one more");
    assert!(
        app.status_line.contains("queue is full"),
        "{}",
        app.status_line
    );
}
#[tokio::test]
async fn a_failed_image_keeps_the_draft() {
    let dir = image_workspace("octet-image-draft", &["notes.txt"]);
    let mut app = app();
    app.composer.root = dir.path().to_path_buf();
    let vendor = RecordingVendor::default();
    assert!(app.composer.editor.insert("/image notes.txt"));
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "/image notes.txt");
    assert!(app.entries_text().contains("not a PNG"));
    app.composer.editor.take();
    std::fs::write(dir.path().join("ok.png"), b"x").unwrap();
    assert!(app.composer.editor.insert("/image ok.png"));
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(app.composer.editor.text().is_empty());
}
#[tokio::test]
async fn steer_while_cancelling_queues_it() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.conn.start_turn();
    cancel_turn(&mut app, &vendor).await;
    steer(&mut app, &vendor, "look at the tests".into());
    assert_eq!(
        app.composer.queue.front(),
        Some(&Prompt::plain("look at the tests".into()))
    );
    assert!(app.entries_text().contains("The turn is stopping"));
    // The turn's end clears the state; the queued prompt then goes.
    app.event(octet_core::Event::Finished {
        outcome: octet_core::Outcome::Interrupted,
    });
    assert!(!app.conn.is_cancelling());
}
#[tokio::test]
async fn one_tab_listing_at_a_time() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    // A listing still stuck on a slow folder from an earlier Tab.
    app.composer
        .listing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(app.composer.editor.insert("/tm"));
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert!(
        app.status_line.starts_with("Still reading"),
        "{}",
        app.status_line
    );
    assert_eq!(app.composer.editor.text(), "/tm");
    // A listing that finishes frees the next Tab.
    app.composer
        .listing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert!(
        !app.composer
            .listing
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}
#[tokio::test]
async fn an_answer_key_right_after_an_approval_is_ignored() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.event(crate::test_support::approval(7, "rm -rf build"));
    // A letter typed as the dialog opens must not answer it.
    key_action(&mut app, &vendor, key(KeyCode::Char('a'))).await;
    assert!(
        vendor.sent.borrow().is_empty(),
        "{:?}",
        vendor.sent.borrow()
    );
    assert_eq!(app.overlay.approvals.len(), 1);
    // Once the dialog has been up a moment, the same key answers.
    // Shown on screen a moment ago.
    app.overlay.approval_shown = std::time::Instant::now().checked_sub(Duration::from_secs(1));
    key_action(&mut app, &vendor, key(KeyCode::Char('a'))).await;
    assert!(matches!(
        vendor.sent.borrow().as_slice(),
        [Command::Answer { id: 7, allow: true }]
    ));
}
#[tokio::test]
async fn esc_while_connecting_does_not_leave_cancelling() {
    let mut app = app();
    // No event returns a live session to connecting without resetting the
    // state under test, so the phase is set.
    app.conn.phase = ConnPhase::Connecting;
    let vendor = RecordingVendor::default();
    key_action(&mut app, &vendor, key(KeyCode::Esc)).await;
    assert_eq!(app.status_line, crate::app::CANCELLING);
    // The cancelled connection reports an error and stops; no turn finishes.
    app.event(octet_core::Event::Error("Connection cancelled".into()));
    app.event(octet_core::Event::Stopped);
    assert_ne!(app.status_line, crate::app::CANCELLING);
}
#[test]
fn paste_keeps_tabs() {
    let mut app = app();
    paste(&mut app, "all:\n\tmake build");
    assert_eq!(app.composer.editor.text(), "all:\n\tmake build");
}
#[test]
fn paste_into_an_overlay_hints() {
    let mut app = app();
    app.overlay.help = true;
    paste(&mut app, "lost");
    assert!(app.status_line.contains("Close"), "{}", app.status_line);
}
#[test]
fn size_label_says_kib() {
    assert_eq!(size_label(2048), "2.0 KiB");
    assert_eq!(size_label(10), "10 B");
}
#[test]
fn a_queued_prompt_that_fails_to_send_is_kept() {
    let mut app = app();
    app.composer
        .queue
        .push_back(crate::vendor::Prompt::plain("next".into()));
    let vendor = RecordingVendor::default();
    vendor.refuse.set(true);
    crate::send_queued(&mut app, &vendor);
    assert_eq!(app.composer.queue.len(), 1, "the prompt was lost");
}
#[tokio::test]
async fn cancel_interrupts_before_saving_the_goal() {
    /// Notes what the goal file said when the interrupt arrived.
    struct Watching {
        file: std::path::PathBuf,
        seen: std::cell::RefCell<String>,
    }
    impl crate::vendor::Vendor for Watching {
        fn send(&self, _: Command) -> Result<(), octet_core::SendError> {
            Ok(())
        }
        fn interrupt(&self) {
            *self.seen.borrow_mut() = std::fs::read_to_string(&self.file).unwrap_or_default();
        }
    }
    let temp = octet_testkit::TempDir::new("octet-cancel-order");
    let mut app = app();
    let store = octet_core::goal::GoalStore::new(temp.path(), std::path::Path::new("/w"));
    app.goals.attach(store).await.unwrap();
    app.goals.start("Ship it").await.unwrap();
    app.goals.goal_prompt_sent();
    let file = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let vendor = Watching {
        file,
        seen: std::cell::RefCell::default(),
    };
    cancel_turn(&mut app, &vendor).await;
    // The cancel went out before the pause was written.
    assert!(
        vendor.seen.borrow().contains("\"active\""),
        "{}",
        vendor.seen.borrow()
    );
}
#[test]
fn accepting_a_model_suggestion_inserts_only_the_name() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    // Codex lists it, so the plain name goes there: no provider needed.
    app.models.catalogs.set(
        octet_core::Engine::CODEX,
        vec![octet_core::ModelInfo {
            selection: "gpt-6-astra".into(),
            id: None,
            name: "GPT-6-Astra".into(),
            description: String::new(),
        }],
        std::time::SystemTime::now(),
    );
    app.composer.editor.set("/model gpt".into());
    app.composer.completion = Some(composer::Completion {
        kind: composer::Kind::Model,
        items: vec!["gpt-6-astra · codex".into()],
        selected: 0,
        start: 7,
    });
    crate::input::accept_completion(&mut app);
    assert_eq!(app.composer.editor.text(), "/model gpt-6-astra ");
}

/// A Claude interface on `opus`, with Claude's live list and Codex's cached one.
fn picker_app() -> App {
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    crate::test_support::idle(&mut app);
    let m = |s: &str, name: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(s.into()),
        name: name.into(),
        description: String::new(),
    };
    app.event(octet_core::Event::Models(vec![
        m("default", "Default"),
        m("opus", "Opus 5.5"),
    ]));
    app.conn.requested_model = Some("opus".into());
    app.models.catalogs.set(
        octet_core::Engine::CODEX,
        vec![
            m("gpt-6-astra", "GPT-6-Astra"),
            m("gpt-5.6-sol", "GPT-5.6-Sol"),
        ],
        std::time::SystemTime::now(),
    );
    app
}
async fn type_keys(app: &mut App, vendor: &RecordingVendor, text: &str) {
    for c in text.chars() {
        key_action(app, vendor, key(KeyCode::Char(c))).await;
    }
}
fn picker(app: &App) -> &composer::Completion {
    let completion = app.composer.completion.as_ref().expect("picker open");
    assert_eq!(completion.kind, composer::Kind::Model);
    completion
}
#[tokio::test]
async fn model_and_enter_opens_the_picker_on_the_current_model() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "/model ");
    let picker = picker(&app);
    assert_eq!(
        picker.items,
        [
            "default · claude · Default",
            "opus · claude · Opus 5.5 ●",
            "gpt-6-astra · codex · GPT-6-Astra",
            "gpt-5.6-sol · codex · GPT-5.6-Sol",
        ]
    );
    assert_eq!(picker.selected, 1, "the current model starts highlighted");
}
#[tokio::test]
async fn typing_after_model_filters_the_picker() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model gp").await;
    let picker = picker(&app);
    assert_eq!(
        picker.items,
        [
            "gpt-6-astra · codex · GPT-6-Astra",
            "gpt-5.6-sol · codex · GPT-5.6-Sol"
        ]
    );
    assert_eq!(picker.selected, 0);
}
#[tokio::test]
async fn enter_on_a_picked_model_switches_to_its_provider() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    let Action::Exit(Exit::Model(selection)) = action else {
        panic!("expected a model switch");
    };
    assert_eq!(selection.provider, octet_core::Engine::CODEX);
    assert_eq!(selection.model.as_deref(), Some("gpt-6-astra"));
}
#[tokio::test]
async fn a_busy_session_refuses_the_pick_and_keeps_the_picker() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model gp").await;
    app.conn.start_turn();
    let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(matches!(action, Action::Continue));
    assert!(app.composer.completion.is_some(), "the picker stays open");
    assert!(!app.status_line.is_empty(), "the refusal is shown");
}
#[tokio::test]
async fn esc_closes_the_picker() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model op").await;
    assert!(app.composer.completion.is_some());
    key_action(&mut app, &vendor, key(KeyCode::Esc)).await;
    assert!(app.composer.completion.is_none());
}
#[tokio::test]
async fn tab_in_the_picker_inserts_only_the_name() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model gp").await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    let action = key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert!(matches!(action, Action::Continue));
    assert_eq!(app.composer.editor.text(), "/model gpt-5.6-sol ");
}
#[tokio::test]
async fn an_unlisted_name_is_sent_as_typed() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model gpt-5.5").await;
    assert!(picker(&app).items.is_empty());
    let Action::Exit(Exit::Model(selection)) =
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await
    else {
        panic!("expected a model switch");
    };
    assert_eq!(
        selection.provider,
        octet_core::Engine::CODEX,
        "by its gpt- prefix"
    );
    assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
}
#[tokio::test]
async fn a_whole_name_after_a_sub_provider_goes_by_the_prefix_rule() {
    // opencode lists openai/gpt-5.5, so the picker shows it for `gpt-5.5`;
    // Enter must still resolve the name as typed, where Codex's `gpt-`
    // prefix comes before the match after `openai/`.
    let mut app = codex_picker_app();
    let m = |s: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(s.into()),
        name: s.into(),
        description: String::new(),
    };
    app.models.catalogs.set(
        octet_core::Engine::OPENCODE,
        vec![m("openai/gpt-5.5"), m("opencode/glm-5.3-flash-free")],
        std::time::SystemTime::now(),
    );
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model gpt-5.5").await;
    assert!(
        !picker(&app).items.is_empty(),
        "the picker shows openai/gpt-5.5"
    );
    let Action::Exit(Exit::Model(selection)) =
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await
    else {
        panic!("expected a model switch");
    };
    assert_eq!(selection.provider, octet_core::Engine::CODEX);
    assert_eq!(selection.model.as_deref(), Some("gpt-5.5"));
    // A whole name no prefix claims still goes to the one ID ending in it.
    app.composer.editor.set(String::new());
    type_keys(&mut app, &vendor, "/model glm-5.3-flash-free").await;
    let Action::Exit(Exit::Model(selection)) =
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await
    else {
        panic!("expected a model switch");
    };
    assert_eq!(selection.provider, octet_core::Engine::OPENCODE);
    assert_eq!(
        selection.model.as_deref(),
        Some("opencode/glm-5.3-flash-free")
    );
}
/// A Codex interface whose list has no `default`, beside Claude's that does.
fn codex_picker_app() -> App {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    let m = |s: &str, name: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(s.into()),
        name: name.into(),
        description: String::new(),
    };
    app.event(octet_core::Event::Models(vec![m(
        "gpt-6-astra",
        "GPT-6-Astra",
    )]));
    app.models.catalogs.set(
        octet_core::Engine::CLAUDE,
        vec![m("default", "Default"), m("opus", "Opus 5.5")],
        std::time::SystemTime::now(),
    );
    app
}
#[tokio::test]
async fn a_typed_default_stays_with_the_current_provider() {
    let mut app = codex_picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model default").await;
    let Action::Exit(Exit::Model(selection)) =
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await
    else {
        panic!("expected a model switch");
    };
    assert_eq!(selection.provider, octet_core::Engine::CODEX);
    assert_eq!(selection.model, None);
}
#[tokio::test]
async fn a_partial_name_takes_the_highlighted_model() {
    let mut app = codex_picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model op").await;
    let Action::Exit(Exit::Model(selection)) =
        key_action(&mut app, &vendor, key(KeyCode::Enter)).await
    else {
        panic!("expected a model switch");
    };
    assert_eq!(selection.provider, octet_core::Engine::CLAUDE);
    assert_eq!(selection.model.as_deref(), Some("opus"));
}
#[tokio::test]
async fn scrolling_keeps_the_picker_highlight() {
    let mut app = picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    key_action(&mut app, &vendor, key(KeyCode::PageUp)).await;
    assert_eq!(picker(&app).selected, 3);
}
#[tokio::test]
async fn tab_on_another_providers_line_keeps_its_provider() {
    let mut app = codex_picker_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/model def").await;
    assert_eq!(picker(&app).items, ["default · claude · Default"]);
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(app.composer.editor.text(), "/model claude default ");
}

/// A Codex interface with a live Codex skill and a cached Claude skill.
fn skills_app() -> App {
    let mut app = app();
    crate::test_support::idle(&mut app);
    app.conn.skills = vec![skill("imagegen", &[], Some("/s/imagegen/SKILL.md"))];
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        Engine::CLAUDE,
        &ws,
        vec![skill("brainstorming", &[], None)],
        std::time::SystemTime::now(),
    );
    app
}
#[tokio::test]
async fn enter_in_the_skills_picker_prepares_the_skill() {
    let mut app = skills_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    let picker = app.composer.completion.as_ref().expect("picker open");
    assert_eq!(picker.kind, composer::Kind::Skill);
    assert!(
        picker.items[0].starts_with("imagegen · codex"),
        "{:?}",
        picker.items
    );
    assert!(
        picker.items[1].starts_with("brainstorming · claude"),
        "{:?}",
        picker.items
    );
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(app.composer.editor.text(), "/imagegen ");
    assert!(app.composer.completion.is_none());
    assert!(
        vendor.sent.borrow().is_empty(),
        "nothing runs until the draft is sent"
    );
    type_keys(&mut app, &vendor, "a cat").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(matches!(
        &vendor.sent.borrow()[0],
        Command::Skill { name, args, .. } if name == "imagegen" && args == "a cat"
    ));
}
#[tokio::test]
async fn the_skills_picker_filters_and_tab_inserts() {
    let mut app = skills_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills image").await;
    let skills = app.composer.completion.as_ref().expect("picker open");
    assert_eq!(skills.kind, composer::Kind::Skill);
    assert_eq!(skills.items.len(), 1, "{:?}", skills.items);
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(app.composer.editor.text(), "/imagegen ");
    assert!(vendor.sent.borrow().is_empty());
}
#[tokio::test]
async fn a_codex_skill_picked_from_claude_only_prepares_a_draft() {
    let mut app = crate::test_support::app_for(Engine::CLAUDE);
    crate::test_support::idle(&mut app);
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        Engine::CODEX,
        &ws,
        vec![skill(
            "documents:documents",
            &[],
            Some("/s/documents/SKILL.md"),
        )],
        std::time::SystemTime::now(),
    );
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills documents:documents").await;
    let picker = app.composer.completion.as_ref().expect("skill picker open");
    assert_eq!(picker.kind, composer::Kind::Skill);
    assert_eq!(picker.items, ["documents:documents · codex"]);
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(app.composer.editor.text(), "/documents:documents ");
    assert_eq!(app.composer.chosen_skill.as_ref().unwrap().0, Engine::CODEX);
    assert!(app.pending_skill.is_none());
    assert!(
        vendor.sent.borrow().is_empty(),
        "picking never runs a skill"
    );
    assert_eq!(
        view::composer_hint(&app, 100),
        (
            "Enter switches to codex, then runs /documents:documents".into(),
            view::AMBER
        )
    );
}
#[tokio::test]
async fn another_providers_skill_is_prepared_then_switches_with_its_arguments() {
    let mut app = skills_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(
        app.composer.editor.text(),
        "/brainstorming ",
        "Tab prepares another provider's too"
    );
    type_keys(&mut app, &vendor, "the cache").await;
    assert_eq!(
        crate::commands::foreign_skill(&app, app.composer.editor.text()),
        Some(Engine::CLAUDE)
    );
    let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(matches!(&action, Action::Exit(Exit::Model(s)) if s.provider == Engine::CLAUDE));
    let run = app.pending_skill.as_ref().expect("waits for the switch");
    assert_eq!(
        (run.skill.name.as_str(), run.args.as_str()),
        ("brainstorming", "the cache")
    );
    assert!(app.composer.editor.text().is_empty());
}
#[tokio::test]
async fn enter_on_another_providers_line_prepares_it_too() {
    let mut app = skills_app();
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(matches!(action, Action::Continue));
    assert_eq!(app.composer.editor.text(), "/brainstorming ");
    assert!(app.pending_skill.is_none());
}
#[tokio::test]
async fn a_picked_skill_named_like_an_octet_command_says_it_cannot_run() {
    let mut app = skills_app();
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        Engine::CLAUDE,
        &ws,
        vec![skill("model", &[], None)],
        std::time::SystemTime::now(),
    );
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills model").await;
    let skills = app.composer.completion.as_ref().expect("picker open");
    assert!(
        skills.items[0].starts_with("model · claude"),
        "{:?}",
        skills.items
    );
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert_eq!(
        app.composer.editor.text(),
        "/skills model",
        "the draft stays"
    );
    assert_eq!(
        app.status_line,
        "/model is an Octet command; the claude skill of that name cannot run from Octet"
    );
    assert!(vendor.sent.borrow().is_empty());
}
#[tokio::test]
async fn an_octet_command_never_switches_provider() {
    let mut app = skills_app();
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        Engine::CLAUDE,
        &ws,
        vec![skill("model", &[], None)],
        std::time::SystemTime::now(),
    );
    assert_eq!(crate::commands::foreign_skill(&app, "/model list"), None);
    assert_eq!(
        crate::commands::foreign_skill(&app, "/imagegen x"),
        None,
        "the current provider's"
    );
    assert_eq!(
        crate::commands::foreign_skill(&app, "hello /brainstorming"),
        None,
        "not a slash draft"
    );
}
#[tokio::test]
async fn a_pending_skill_runs_once_the_switch_is_ready() {
    let mut app = skills_app();
    app.pending_skill = Some(crate::app::SkillRun {
        engine: Engine::CLAUDE,
        skill: skill("brainstorming", &[], None),
        args: "the cache".into(),
    });
    // The kept interface connects to Claude; on ready, the skill runs there.
    app.connection(
        &octet_core::Config::new(Engine::CLAUDE, "claude", "/tmp"),
        "journal".into(),
    );
    let claude = RecordingVendor::default();
    crate::session_event(
        &mut app,
        &claude,
        octet_core::Event::Ready {
            session: String::new(),
        },
    )
    .await;
    assert!(matches!(
        &claude.sent.borrow()[0],
        Command::Skill { name, args, .. } if name == "brainstorming" && args == "the cache"
    ));
    assert!(app.pending_skill.is_none());
}
#[tokio::test]
async fn a_failed_switch_drops_the_pending_skill() {
    let mut app = skills_app();
    app.pending_skill = Some(crate::app::SkillRun {
        engine: Engine::CLAUDE,
        skill: skill("brainstorming", &[], None),
        args: String::new(),
    });
    app.connection(
        &octet_core::Config::new(Engine::CLAUDE, "claude", "/tmp"),
        "journal".into(),
    );
    let claude = RecordingVendor::default();
    crate::session_event(
        &mut app,
        &claude,
        octet_core::Event::Error("claude: not found".into()),
    )
    .await;
    crate::session_event(&mut app, &claude, octet_core::Event::Stopped).await;
    assert!(app.pending_skill.is_none());
    assert!(
        app.entries_text()
            .contains("The skill /brainstorming was not run: the switch to claude failed"),
        "{}",
        app.entries_text()
    );
    // A later ready (after /reconnect) runs nothing.
    app.connection(
        &octet_core::Config::new(Engine::CLAUDE, "claude", "/tmp"),
        "journal".into(),
    );
    crate::session_event(
        &mut app,
        &claude,
        octet_core::Event::Ready {
            session: String::new(),
        },
    )
    .await;
    assert!(claude.sent.borrow().is_empty());
}
#[tokio::test]
async fn ctrl_o_flips_tool_details_for_every_tools_entry() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.event(crate::test_support::tool(
        "1",
        "Read",
        "a.rs",
        octet_core::ToolPhase::Done,
    ));
    app.event(octet_core::Event::Text("x".into()));
    app.event(crate::test_support::tool(
        "2",
        "Read",
        "b.rs",
        octet_core::ToolPhase::Done,
    ));
    key_action(&mut app, &vendor, ctrl('o')).await;
    assert!(app.chat.tools_expanded);
    assert!(
        app.chat
            .entries
            .iter()
            .filter(|e| e.role == crate::app::Role::Tool)
            .all(|e| e.expanded)
    );
    assert_eq!(app.status_line, "Tool details shown");
    key_action(&mut app, &vendor, ctrl('o')).await;
    assert!(!app.chat.tools_expanded);
    assert_eq!(app.status_line, "Tool details hidden");
    app.event(crate::test_support::tool(
        "3",
        "Read",
        "c.rs",
        octet_core::ToolPhase::Done,
    ));
    assert!(
        !app.chat.entries.back().unwrap().expanded,
        "a new entry follows the setting"
    );
}
#[tokio::test]
async fn typing_filters_the_palette_and_enter_runs_the_match() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    key_action(
        &mut app,
        &vendor,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
    )
    .await;
    type_keys(&mut app, &vendor, "edit").await;
    assert_eq!(app.overlay.palette_query, "edit");
    let names: Vec<&str> = view::palette_matches(&app.overlay.palette_query)
        .into_iter()
        .map(|i| view::palette_entries().nth(i).unwrap().0)
        .collect();
    assert!(names.contains(&"Ctrl+G"), "{names:?}");
    assert!(names.iter().all(|n| {
        let (name, summary) = view::palette_entries().find(|(k, _)| k == n).unwrap();
        format!("{name} {summary}").to_lowercase().contains("edit")
    }));
    key_action(&mut app, &vendor, key(KeyCode::Backspace)).await;
    assert_eq!(app.overlay.palette_query, "edi");
    assert_eq!(app.overlay.selection, 0, "a change selects the first match");
}
#[test]
fn the_palette_filter_finds_model_and_mode_from_mod() {
    let names: Vec<&str> = view::palette_matches("mod")
        .into_iter()
        .map(|i| view::palette_entries().nth(i).unwrap().0)
        .collect();
    assert!(names.contains(&"/model"), "{names:?}");
    assert!(names.contains(&"/mode"), "{names:?}");
}
#[tokio::test]
async fn opening_the_palette_starts_unfiltered() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    app.overlay.palette_query = "zz".into();
    key_action(
        &mut app,
        &vendor,
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
    )
    .await;
    assert!(app.overlay.palette_query.is_empty());
}
#[tokio::test]
async fn f2_hides_and_shows_the_task_sidebar() {
    let mut app = app();
    let vendor = RecordingVendor::default();
    assert!(app.sidebar);
    key_action(&mut app, &vendor, key(KeyCode::F(2))).await;
    assert!(!app.sidebar);
    key_action(&mut app, &vendor, key(KeyCode::F(2))).await;
    assert!(app.sidebar);
}
#[tokio::test]
async fn f2_on_a_narrow_terminal_says_when_the_sidebar_shows() {
    let vendor = RecordingVendor::default();
    for (width, hint) in [(100, "The Task sidebar shows from 112 columns"), (120, "")] {
        let mut app = app();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
        terminal
            .draw(|frame| crate::view::draw(frame, &mut app))
            .unwrap();
        key_action(&mut app, &vendor, key(KeyCode::F(2))).await;
        assert!(!app.sidebar, "the state flips either way");
        assert_eq!(app.status_line, hint, "{width} columns");
    }
}

#[tokio::test]
async fn a_picked_skill_keeps_its_provider_when_both_have_the_name() {
    let mut app = skills_app();
    app.conn.skills = vec![skill("brainstorming", &[], Some("/s/codex/SKILL.md"))];
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    let selected = &app.composer.completion.as_ref().unwrap().items[1];
    assert!(selected.contains("claude"), "{selected}");
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    type_keys(&mut app, &vendor, "the cache").await;
    let action = key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    assert!(
        matches!(action, Action::Exit(Exit::Model(s)) if s.provider == Engine::CLAUDE),
        "selected Claude but sent to current provider: {:?}",
        vendor.sent.borrow()
    );
}
#[tokio::test]
async fn a_picked_skill_shows_its_switch_and_is_forgotten_when_the_draft_changes() {
    let mut app = skills_app();
    app.conn.skills = vec![skill("brainstorming", &[], Some("/s/codex/SKILL.md"))];
    let vendor = RecordingVendor::default();
    type_keys(&mut app, &vendor, "/skills").await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    key_action(&mut app, &vendor, key(KeyCode::Down)).await;
    key_action(&mut app, &vendor, key(KeyCode::Enter)).await;
    // The current provider has the name too, but the pick was Claude's: the hint says so.
    assert_eq!(
        crate::commands::foreign_skill(&app, app.composer.editor.text()),
        Some(Engine::CLAUDE)
    );
    key_action(&mut app, &vendor, ctrl('u')).await;
    assert!(
        app.composer.chosen_skill.is_none(),
        "an emptied draft forgets the pick"
    );
    type_keys(&mut app, &vendor, "/brainstorming").await;
    assert_eq!(
        crate::commands::foreign_skill(&app, app.composer.editor.text()),
        None,
        "typed again by hand, the current provider's runs"
    );
}
#[tokio::test]
async fn tab_on_a_name_two_providers_list_shows_the_bracketed_row() {
    // Codex lists gpt-5.5 and opencode lists openai/gpt-5.5, the same model.
    let mut app = crate::test_support::app_for(Engine::CODEX);
    crate::test_support::idle(&mut app);
    let m = |s: &str, name: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(s.into()),
        name: name.into(),
        description: String::new(),
    };
    app.event(octet_core::Event::Models(vec![
        m("gpt-5.5", "GPT-5.5"),
        m("gpt-5.4", "GPT-5.4"),
    ]));
    app.models.catalogs.set(
        Engine::OPENCODE,
        vec![m("openai/gpt-5.5", "OpenAI/GPT-5.5")],
        std::time::SystemTime::now(),
    );
    let vendor = RecordingVendor::default();
    // The provider form has no picker: Tab offers that provider's names.
    app.insert_or_warn("/model codex gpt-5.");
    assert!(app.composer.completion.is_none());
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    let popup = app.composer.completion.as_ref().expect("Tab offers names");
    assert_eq!(popup.items, ["gpt-5.5 (codex)", "gpt-5.4 · codex"]);
    // Typing on narrows it; a draft that leaves the form closes it.
    key_action(&mut app, &vendor, key(KeyCode::Char('5'))).await;
    let popup = app.composer.completion.as_ref().expect("still open");
    assert_eq!(popup.items, ["gpt-5.5 (codex)"]);
    // Backspace widens it again, from the provider's whole list.
    key_action(&mut app, &vendor, key(KeyCode::Backspace)).await;
    let popup = app.composer.completion.as_ref().expect("still open");
    assert_eq!(popup.items, ["gpt-5.5 (codex)", "gpt-5.4 · codex"]);
    key_action(&mut app, &vendor, key(KeyCode::Char('5'))).await;
    key_action(&mut app, &vendor, key(KeyCode::Char(' '))).await;
    assert!(app.composer.completion.is_none());
}
#[tokio::test]
async fn a_tab_line_in_the_provider_form_never_names_the_provider_twice() {
    // Codex and opencode both list shared-x, so the name alone is
    // ambiguous from Claude; the draft already names Codex.
    let mut app = crate::test_support::app_for(Engine::CLAUDE);
    crate::test_support::idle(&mut app);
    let m = |s: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(s.into()),
        name: s.into(),
        description: String::new(),
    };
    app.event(octet_core::Event::Models(vec![m("opus")]));
    let now = std::time::SystemTime::now();
    app.models
        .catalogs
        .set(Engine::CODEX, vec![m("shared-x"), m("shared-y")], now);
    app.models
        .catalogs
        .set(Engine::OPENCODE, vec![m("shared-x")], now);
    let vendor = RecordingVendor::default();
    app.insert_or_warn("/model codex shared-");
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(
        app.composer
            .completion
            .as_ref()
            .expect("Tab offers names")
            .items,
        ["shared-x (codex)", "shared-y · codex"]
    );
    key_action(&mut app, &vendor, key(KeyCode::Tab)).await;
    assert_eq!(app.composer.editor.text(), "/model codex shared-x ");
}
