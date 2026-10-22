use super::*;
use crate::test_support::{approval, tool};
use octet_core::{Event, ToolPhase};
use ratatui::{Terminal, backend::TestBackend};
fn screen(width: u16, height: u16, app: &mut App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}
/// A Claude interface with a conversation that fills the width.
fn talkative_app() -> App {
    let c = octet_core::Config::new(octet_core::Engine::CLAUDE, "claude", "/tmp/project");
    let mut app = App::new(&c, "journal".into());
    crate::test_support::idle(&mut app);
    for n in 0..6 {
        app.event(Event::User(format!(
            "prompt {n} with a long line of text that goes on and on to fill the width"
        )));
        app.event(Event::Text(format!(
            "reply {n} with a long line of text that goes on and on to fill the width"
        )));
    }
    app
}
#[test]
fn the_model_picker_is_as_wide_as_its_longest_line() {
    let mut app = talkative_app();
    let long = octet_core::ModelInfo {
        selection: "openai/gpt-5.4-mini-fast".into(),
        id: Some("openai/gpt-5.4-mini-fast".into()),
        name: "OpenAI/GPT-5.4 mini Fast (priority tier)".into(),
        description: String::new(),
    };
    app.models.catalogs.set(
        octet_core::Engine::OPENCODE,
        vec![long],
        std::time::SystemTime::now(),
    );
    app.composer.editor.set("/model openai/gpt-5.4-mini".into());
    crate::input::refresh_model_picker(&mut app);
    let rows = screen(160, 50, &mut app);
    assert!(
        rows.iter()
            .any(|row| row.contains("OpenAI/GPT-5.4 mini Fast (priority tier)")),
        "{}",
        rows.join("\n")
    );
}
#[test]
fn the_approval_dialog_is_framed_apart_from_the_conversation() {
    let mut app = talkative_app();
    app.event(approval(7, "Write /tmp/project/hello.txt"));
    let rows = screen(160, 50, &mut app);
    // Its own border sets it apart from the conversation it covers.
    let heading = rows
        .iter()
        .position(|row| row.contains("Permission required"))
        .expect("the dialog shows");
    assert!(
        rows[heading].contains("│ Permission required"),
        "{}",
        rows.join("\n")
    );
    assert!(rows[heading - 1].contains("╭─"), "{}", rows.join("\n"));
}
#[test]
fn following_the_latest_cancels_a_pending_catalog_scroll() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.conn.models = (0..30)
        .map(|i| octet_core::ModelInfo {
            selection: format!("m{i}"),
            id: Some(format!("m{i}")),
            name: format!("Model {i}"),
            description: String::new(),
        })
        .collect();
    app.show_models(1);
    app.follow_latest();
    screen(120, 36, &mut app);
    assert_eq!(
        app.chat.scroll, 0,
        "Ctrl+End before the next frame still wins"
    );
}
#[test]
fn the_popup_keeps_the_selected_row_in_sight_on_a_short_screen() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.composer.completion = Some(crate::composer::Completion {
        kind: crate::composer::Kind::File,
        items: (0..8).map(|i| format!("file{i}.rs")).collect(),
        selected: 7,
        moved: false,
        start: 0,
    });
    let rows = screen(80, 14, &mut app);
    assert!(
        rows.iter().any(|row| row.contains(" › file7.rs")),
        "{}",
        rows.join("\n")
    );
}
#[test]
fn the_popup_lists_suggestions_above_the_prompt() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.composer.completion = Some(crate::composer::Completion {
        kind: crate::composer::Kind::File,
        items: vec!["src/main.rs".into(), "src/model.rs".into()],
        selected: 1,
        moved: false,
        start: 0,
    });
    let rows = screen(100, 30, &mut app);
    assert!(rows.iter().any(|row| row.contains("   src/main.rs")));
    assert!(rows.iter().any(|row| row.contains(" › src/model.rs")));
    app.composer.completion.as_mut().unwrap().items.clear();
    let rows = screen(100, 30, &mut app);
    assert!(rows.iter().any(|row| row.contains("Indexing files…")));
}
#[test]
fn the_task_sidebar_shows_goal_now_and_queue() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    crate::test_support::idle(&mut app);
    app.composer
        .queue
        .push_back(crate::vendor::Prompt::plain("then add tests\nmore".into()));
    let rows = screen(120, 40, &mut app);
    let text = rows.join("\n");
    assert!(text.contains("╭ Task"), "{text}");
    for expected in [
        "GOAL",
        "No goal · /goal sets one",
        "NOW",
        "Idle",
        "QUEUED",
        "then add tests",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    for gone in ["ENGINE", "SESSION", "QUICK COMMANDS"] {
        assert!(!text.contains(gone), "{gone}");
    }
    app.sidebar = false;
    let rows = screen(120, 40, &mut app);
    assert!(!rows.join("\n").contains("╭ Task"));
}
#[test]
fn the_task_sidebar_shows_the_goal_and_its_turns() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    crate::test_support::idle(&mut app);
    let goal = octet_core::goal::Goal::new("Make every test pass on Linux and macOS").unwrap();
    app.goals.set_goal(Some(goal));
    let text = screen(120, 40, &mut app).join("\n");
    let turns = format!("/{} turns", octet_core::goal::MAX_GOAL_TURNS);
    for expected in ["Make every test", "Linux and macOS", turns.as_str()] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(!text.contains("No goal"), "{text}");
}
#[test]
fn an_empty_composer_gives_rows_back_to_the_conversation() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::User("hello".into()));
    let rows = screen(44, 16, &mut app);
    // Compact: no bottom margin, so the border sits 4 rows above the status line.
    assert!(rows[16 - 5].starts_with(" ╭ Prompt"), "{}", rows[16 - 5]);
}
#[test]
fn help_keeps_its_last_line_on_narrow_and_short_terminals() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.overlay.help = true;
    for (width, height) in [(100, 30), (75, 40), (70, 40), (60, 40), (100, 32), (80, 24)] {
        // How to close is always on the border, whatever fits inside.
        app.overlay.help_scroll = 0;
        let rows = screen(width, height, &mut app);
        assert!(
            rows.iter().any(|row| row.contains("Esc closes")),
            "{width}x{height}:\n{}",
            rows.join("\n")
        );
        // Scrolling reaches the last line.
        app.overlay.help_scroll = u16::MAX;
        let rows = screen(width, height, &mut app);
        assert!(
            rows.iter().any(|row| row.contains("Esc or F1 closes help")),
            "{width}x{height} scrolled:\n{}",
            rows.join("\n")
        );
    }
}
#[test]
fn help_names_the_command_palette_key() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.overlay.help = true;
    let rows = screen(100, 32, &mut app);
    assert!(
        rows.iter().any(|row| row.contains("Ctrl+P commands")),
        "{}",
        rows.join("\n")
    );
    // The help scrolls as commands are added; its border always says how to close it.
    assert!(rows.iter().any(|row| row.contains("Esc closes")));
}
#[test]
fn the_draft_shows_at_the_minimum_size() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert!(app.composer.editor.insert("VISIBLE"));
    for width in [38, 80] {
        let rows = screen(width, 12, &mut app);
        assert!(
            rows.iter().any(|row| row.contains("VISIBLE")),
            "{width}x12:\n{}",
            rows.join("\n")
        );
    }
}
#[test]
fn help_names_the_tool_details_key() {
    assert!(
        help_lines()
            .iter()
            .any(|l| l.contains("Ctrl+O show or hide tool details"))
    );
}
#[test]
fn the_palette_shows_every_command_and_its_keys() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.overlay.palette = true;
    let mut columns = Vec::new();
    // The list scrolls to keep the selected row in sight, so every entry
    // is reachable at 80x24.
    let mut rows = Vec::new();
    for (index, (name, description)) in palette_entries().enumerate() {
        app.overlay.selection = index;
        rows = screen(80, 24, &mut app);
        // " /mode " must not match the "/model" row.
        let row = rows
            .iter()
            .find(|row| row.contains(&format!(" {name} ")))
            .unwrap_or_else(|| panic!("{name} missing"));
        assert!(row.contains(description), "{name}: description cut: {row}");
        // Columns, not bytes: the selected row's "›" is three bytes wide.
        columns.push(
            row.find(description)
                .map(|byte| row[..byte].chars().count()),
        );
    }
    assert!(
        columns.windows(2).all(|pair| pair[0] == pair[1]),
        "descriptions start in different columns: {columns:?}"
    );
    assert!(rows.iter().any(|row| row.contains("Esc close")));
    assert!(
        rows.iter().any(|row| row.contains("Ctrl+O")),
        "Ctrl+O row missing"
    );
}
#[test]
fn empty_conversation_leaves_the_centre_blank() {
    let config = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    for (width, height) in [(80, 24), (60, 20)] {
        let mut app = App::new(&config, "journal".into());
        app.monochrome = false;
        app.event(Event::Ready {
            session: "demo".into(),
        });
        let rows = screen(width, height, &mut app);
        // Rows between the 3-row banner (after the margin) and the empty
        // 4-row composer, the status line and the bottom margin.
        for row in &rows[4..rows.len() - 6] {
            let inside: String = row.chars().skip(1).take(width as usize - 2).collect();
            assert!(inside.trim().is_empty(), "{width}×{height}: {row:?}");
        }
    }
}
#[test]
fn header_banner_shows_mini_version_mode_activity_and_workspace() {
    let config = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    for (width, height) in [(160, 42), (80, 30)] {
        let mut app = App::new(&config, "journal".into());
        app.monochrome = false;
        for event in [
            Event::Ready {
                session: "demo".into(),
            },
            Event::User("fix the failing test".into()),
            Event::Started,
            tool("1", "Bash", "", ToolPhase::Running),
        ] {
            app.event(event);
        }
        let rows = screen(width, height, &mut app);
        // The header starts inside the one-cell margin.
        for row in &rows[1..4] {
            let mini: String = row.chars().skip(1).take(9).collect();
            assert_eq!(mini, "▀".repeat(9), "{width}×{height}: {row}");
        }
        let at = |row: usize| format!("{width}×{height}: {}", rows[row]);
        let version = format!("Octet v{}", env!("CARGO_PKG_VERSION"));
        assert!(rows[1].contains(&version), "{}", at(1));
        assert!(rows[1].contains("working"), "{}", at(1));
        assert!(rows[2].contains("ask"), "{}", at(2));
        assert!(
            rows[3].contains("CODING") && rows[3].contains("/tmp"),
            "{}",
            at(3)
        );
    }
}
#[test]
fn permission_mode_stays_visible_on_narrow_headers() {
    let config = octet_core::Config {
        model: Some("gpt-5.5-codex-max-preview-long-name".into()),
        mode: octet_core::Mode::FullAccess,
        ..octet_core::Config::new(octet_core::Engine::CODEX, "codex", "/tmp")
    };
    for width in [80, 60, 40, 38] {
        let mut app = App::new(&config, "journal".into());
        app.monochrome = false;
        let rows = screen(width, 24, &mut app);
        assert!(
            rows[0].contains("full-access"),
            "{width} columns: {}",
            rows[0]
        );
    }
}
#[test]
fn codex_header_distinguishes_running_permissions_from_next_turn() {
    use octet_core::{Engine, Mode, Outcome};
    for width in [120, 80, 40, 38] {
        let mut app = crate::test_support::app_for(Engine::CODEX);
        crate::test_support::idle(&mut app);
        app.event(Event::Started);
        app.event(Event::ModeChanged(Mode::Auto));
        assert_eq!(mode_chip(&app).content, "ask → auto next");
        assert_eq!(mode_chip(&app).style.fg, Some(MUTED));
        assert!(app.mode_details().contains("running turn still uses ask"));
        assert!(
            app.mode_details()
                .contains("Steering does not start a new turn")
        );
        let rows = screen(width, 24, &mut app);
        assert!(
            rows.iter().any(|row| row.contains("ask → auto next")),
            "{rows:?}"
        );
        // Steering and another mode selection do not update the running turn.
        app.event(Event::User("keep going".into()));
        app.event(Event::ModeChanged(Mode::AcceptEdits));
        assert_eq!(mode_chip(&app).content, "ask → accept-edits next");
        app.event(Event::ModeChanged(Mode::Ask));
        assert_eq!(mode_chip(&app).content, "ask");
        app.event(Event::ModeChanged(Mode::Auto));
        app.event(Event::Finished {
            outcome: Outcome::Completed,
        });
        app.event(Event::Started);
        assert_eq!(mode_chip(&app).content, "auto");
        app.event(Event::ModeChanged(Mode::Ask));
        assert_eq!(mode_chip(&app).content, "auto → ask next");
        app.event(Event::Stopped);
        assert_eq!(mode_chip(&app).content, "ask");
    }
}

#[test]
fn opencode_header_distinguishes_running_permissions_from_next_turn() {
    use octet_core::{Engine, Mode, Outcome};
    let mut app = crate::test_support::app_for(Engine::OPENCODE);
    crate::test_support::idle(&mut app);
    app.event(Event::Started);
    app.event(Event::ModeChanged(Mode::AcceptEdits));
    // opencode applies an agent at the next prompt, as Codex does.
    assert_eq!(mode_chip(&app).content, "ask → accept-edits next");
    assert!(app.mode_details().contains("running turn still uses ask"));
    let rows = screen(80, 24, &mut app);
    assert!(
        rows.iter()
            .any(|row| row.contains("ask → accept-edits next")),
        "{rows:?}"
    );
    app.event(Event::Finished {
        outcome: Outcome::Completed,
    });
    app.event(Event::Started);
    assert_eq!(mode_chip(&app).content, "accept-edits");
    assert_eq!(app.conn.mode_for_turn, None);
}

#[test]
fn opencode_switch_sent_while_idle_applies_to_the_prompt_sent_behind_it() {
    use octet_core::{Engine, Mode};
    let vendor = crate::vendor::RecordingVendor::default();
    let mut app = crate::test_support::app_for(Engine::OPENCODE);
    crate::test_support::idle(&mut app);
    crate::commands::switch_mode(&mut app, &vendor, Mode::AcceptEdits);
    // A prompt follows before opencode confirms; the interface marks it
    // running at once, but the driver holds it until the switch lands.
    app.conn.start_turn();
    app.event(Event::ModeChanged(Mode::AcceptEdits));
    assert_eq!(mode_chip(&app).content, "accept-edits");
    assert_eq!(app.conn.mode_for_turn, None);
    assert!(!app.mode_details().contains("running turn still uses"));
    app.event(Event::Started);
    assert_eq!(mode_chip(&app).content, "accept-edits");
}

#[test]
fn opencode_switch_sent_mid_turn_waits_for_the_next_turn() {
    use octet_core::{Engine, Mode};
    let vendor = crate::vendor::RecordingVendor::default();
    let mut app = crate::test_support::app_for(Engine::OPENCODE);
    crate::test_support::idle(&mut app);
    app.event(Event::Started);
    crate::commands::switch_mode(&mut app, &vendor, Mode::AcceptEdits);
    app.event(Event::ModeChanged(Mode::AcceptEdits));
    assert_eq!(mode_chip(&app).content, "ask → accept-edits next");
    assert!(app.mode_details().contains("running turn still uses ask"));
    // opencode cannot steer, so the details do not talk about steering.
    assert!(
        !app.mode_details().contains("Steering"),
        "{}",
        app.mode_details()
    );
}

#[test]
fn a_late_confirmation_does_not_reuse_an_answered_switchs_turn_state() {
    use octet_core::{Engine, Mode};
    let vendor = crate::vendor::RecordingVendor::default();
    let mut app = crate::test_support::app_for(Engine::OPENCODE);
    crate::test_support::idle(&mut app);
    // Switch A is reported unconfirmed: the header goes back to ask, and
    // what was recorded for A is spent.
    crate::commands::switch_mode(&mut app, &vendor, Mode::AcceptEdits);
    app.event(Event::ModeChanged(Mode::Ask));
    assert_eq!(app.conn.mode_sent_mid_turn, None);
    // Switch B is sent while idle, and a prompt follows it.
    crate::commands::switch_mode(&mut app, &vendor, Mode::Auto);
    app.conn.start_turn();
    // A's late confirmation arrives: no "next turn" display.
    app.event(Event::ModeChanged(Mode::AcceptEdits));
    assert_eq!(mode_chip(&app).content, "accept-edits");
    assert_eq!(app.conn.mode_for_turn, None);
    assert_eq!(app.conn.mode_sent_mid_turn, None);
}

#[test]
fn a_late_confirmation_after_a_refused_prompt_is_a_plain_mode_change() {
    use octet_core::{Engine, Mode, Outcome};
    let vendor = crate::vendor::RecordingVendor::default();
    let mut app = crate::test_support::app_for(Engine::OPENCODE);
    crate::test_support::idle(&mut app);
    // The switch times out; the prompt sent next is refused unsent, as
    // the driver does while the mode is unknown.
    crate::commands::switch_mode(&mut app, &vendor, Mode::AcceptEdits);
    app.event(Event::ModeChanged(Mode::Ask));
    app.conn.start_turn();
    app.event(Event::User("hello".into()));
    app.event(Event::Started);
    app.event(Event::Error("Not sent: the mode is unknown".into()));
    app.event(Event::Finished {
        outcome: Outcome::Failed,
    });
    // No turn runs when the late confirmation comes: no "next turn".
    app.event(Event::ModeChanged(Mode::AcceptEdits));
    assert_eq!(mode_chip(&app).content, "accept-edits");
    assert_eq!(app.conn.mode_for_turn, None);
}

#[test]
fn every_provider_says_whether_a_mode_waits_for_the_next_turn() {
    use octet_core::Engine;
    let next_turn = |engine: Engine| engine.provider().mode_applies_next_turn;
    assert!(next_turn(Engine::CODEX));
    assert!(next_turn(Engine::OPENCODE));
    assert!(!next_turn(Engine::CLAUDE));
    assert!(!next_turn(Engine::DEMO));
}

#[test]
fn claude_mode_confirmation_updates_the_running_turn() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CLAUDE);
    crate::test_support::idle(&mut app);
    app.event(Event::Started);
    app.event(Event::ModeChanged(octet_core::Mode::Auto));
    assert_eq!(mode_chip(&app).content, "auto");
    assert_eq!(app.conn.mode_for_turn, None);
}

#[test]
fn codex_mode_confirmed_before_started_applies_to_the_new_turn() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    // The UI marks a submitted prompt running before driver events arrive.
    app.conn.start_turn();
    app.event(Event::ModeChanged(octet_core::Mode::Auto));
    app.event(Event::Started);
    assert_eq!(mode_chip(&app).content, "auto");
    assert_eq!(app.conn.mode_for_turn, None);
}
#[test]
fn status_shows_in_full_on_narrow_terminals() {
    let config = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    for width in [59, 50, 40] {
        let mut app = App::new(&config, "journal".into());
        app.monochrome = false;
        for event in [
            Event::Ready {
                session: "demo".into(),
            },
            Event::Started,
            Event::Finished {
                outcome: octet_core::Outcome::Interrupted,
            },
        ] {
            app.event(event);
        }
        let rows = screen(width, 12, &mut app);
        assert!(rows[0].contains("● interrupted"), "{width}: {}", rows[0]);
    }
}
#[test]
fn no_color_keeps_the_text_header() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = true;
    for (height, row) in [(24, 0), (42, 1)] {
        let rows = screen(80, height, &mut app);
        let mark = if height < 30 {
            "◇ Octet".to_owned()
        } else {
            format!("◇ Octet v{}", env!("CARGO_PKG_VERSION"))
        };
        assert!(rows[row].contains(&mark), "{}", rows[row]);
        assert!(!rows[..4].iter().any(|row| row.contains('▀')));
    }
}
#[test]
fn mascot_tracks_activity_and_keeps_errors_visible_after_disconnect() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = false;
    for (event, expected) in [
        (
            Event::Ready {
                session: "demo".into(),
            },
            "IDLE",
        ),
        (Event::Started, "THINKING"),
        (
            tool("1", "Read", "src/main.rs", ToolPhase::Running),
            "SEARCHING",
        ),
        (
            tool("2", "Bash", "cargo check", ToolPhase::Running),
            "CODING",
        ),
        (
            tool("3", "dispatch_subagent", "", ToolPhase::Running),
            "DELEGATING",
        ),
        (approval(1, "Write src/main.rs"), "APPROVAL"),
        (Event::ApprovalClosed(1), "DELEGATING"),
        (
            Event::Finished {
                outcome: octet_core::Outcome::Completed,
            },
            "SUCCESS",
        ),
        (
            Event::Finished {
                outcome: octet_core::Outcome::Interrupted,
            },
            "SLEEPING",
        ),
        (Event::Error("provider disconnected".into()), "ERROR"),
        (Event::Stopped, "ERROR"),
    ] {
        app.event(event);
        // The banner's third row starts with the activity name; the
        // transcript's own ERROR label must not satisfy this check.
        let rows = screen(160, 42, &mut app);
        let activity = rows[3].chars().skip(11).collect::<String>();
        assert!(
            activity.trim_start().starts_with(expected),
            "mascot should show {expected}: {}",
            rows[3]
        );
    }
}
#[test]
fn a_warning_is_labelled_and_on_the_status_line() {
    let c = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp/project");
    let mut app = App::new(&c, "journal".into());
    app.warn("Full access carried to Claude: no permission checks.");
    let rows = screen(100, 30, &mut app);
    let text = rows.join("\n");
    assert!(text.contains("WARNING"), "{text}");
    assert!(text.contains("Full access carried to Claude"), "{text}");
    assert_eq!(
        app.status_line,
        "Full access carried to Claude: no permission checks."
    );
}
#[test]
fn renders_narrow_wide_and_approval_without_panics() {
    for (w, h) in [(30, 8), (40, 12), (80, 24), (120, 36)] {
        let c = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp/project");
        let mut a = App::new(&c, "journal".into());
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, &mut a)).unwrap();
        a.event(approval(7, "edit hello.txt"));
        t.draw(|f| draw(f, &mut a)).unwrap();
    }
}

#[test]
fn composer_grows_with_the_draft() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert_eq!(
        composer_height(app.composer.editor.layout(draft_width(80)).0.len(), false),
        4
    );
    assert!(app.composer.editor.insert("one\ntwo\nthree"));
    assert_eq!(
        composer_height(app.composer.editor.layout(draft_width(80)).0.len(), false),
        6
    );
}
#[test]
fn composer_counts_wrapped_rows() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    // 80 columns leave 74 for text: 100 characters wrap onto a second row.
    assert!(app.composer.editor.insert(&"x".repeat(100)));
    assert_eq!(
        composer_height(app.composer.editor.layout(draft_width(80)).0.len(), false),
        5
    );
}
#[test]
fn composer_height_is_capped() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert!(app.composer.editor.insert(&"line\n".repeat(40)));
    assert_eq!(
        composer_height(app.composer.editor.layout(draft_width(80)).0.len(), false),
        7
    );
}
#[test]
fn the_hint_row_matches_what_enter_does() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    assert_eq!(
        composer_hint(&app, 100).0,
        "Enter send · Alt+Enter newline · ↑ history · Ctrl+P commands"
    );
    // A phone rarely sends Alt+Enter; Ctrl+J always arrives.
    assert_eq!(composer_hint(&app, 60).0, "Enter send · Ctrl+J newline");
    app.event(Event::Started);
    assert_eq!(
        composer_hint(&app, 100).0,
        "Enter queue · Esc cancel turn · Ctrl+P commands"
    );
    assert_eq!(composer_hint(&app, 60).0, "Enter queue · Esc cancel turn");
    let rows = screen(100, 30, &mut app);
    assert!(
        rows.iter().any(|r| r.contains("╭ Next prompt ─")),
        "{}",
        rows.join("\n")
    );
    assert!(
        !rows.iter().any(|r| r.contains("Enter send")),
        "no contradicting hint"
    );
}
#[test]
fn the_hint_warns_before_a_provider_switch() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        octet_core::Engine::CLAUDE,
        &ws,
        vec![octet_core::SkillInfo {
            name: "review".into(),
            description: String::new(),
            aliases: Vec::new(),
            argument_hint: String::new(),
            path: None,
        }],
        std::time::SystemTime::now(),
    );
    assert!(app.composer.editor.insert("/review src"));
    assert_eq!(
        composer_hint(&app, 100),
        ("Enter switches to claude, then runs /review".into(), AMBER)
    );
}
#[test]
fn mid_turn_the_hint_says_the_switch_waits() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    crate::test_support::idle(&mut app);
    let ws = app.workspace().to_path_buf();
    app.models.skill_catalogs.set(
        octet_core::Engine::CLAUDE,
        &ws,
        vec![octet_core::SkillInfo {
            name: "review".into(),
            description: String::new(),
            aliases: Vec::new(),
            argument_hint: String::new(),
            path: None,
        }],
        std::time::SystemTime::now(),
    );
    app.event(Event::Started);
    assert!(app.composer.editor.insert("/review src"));
    assert_eq!(
        composer_hint(&app, 100),
        ("Finish the turn to switch to claude".into(), AMBER)
    );
}
#[test]
fn chips_have_their_own_row_and_grow_the_box() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    assert_eq!(composer_height(1, false), 4);
    assert_eq!(composer_height(1, true), 5);
    app.attach(crate::shell::Ran {
        command: "ls".into(),
        status: crate::shell::Status::Exited(0),
        output: "a\n".into(),
    });
    app.composer
        .queue
        .push_back(crate::vendor::Prompt::plain("later".into()));
    assert_eq!(chips(&app).as_deref(), Some("+ ls (exit 0) · 1 queued"));
    let rows = screen(100, 30, &mut app);
    let title = rows.iter().position(|r| r.contains("╭ Prompt ─")).unwrap();
    assert!(
        rows[title + 2].contains("+ ls (exit 0) · 1 queued"),
        "{}",
        rows.join("\n")
    );
}
#[test]
fn chips_and_hint_fit_at_the_minimum_size() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.composer
        .queue
        .push_back(crate::vendor::Prompt::plain("later".into()));
    assert!(app.composer.editor.insert("draft"));
    // The compact layout leaves the box room for the draft, the chips and the hint.
    let text = screen(38, 12, &mut app).join("\n");
    assert!(text.contains("│ draft"), "{text}");
    assert!(
        text.contains("1 queued") && text.contains("Enter send"),
        "{text}"
    );
}
#[test]
fn chips_give_way_in_a_box_with_one_inner_row() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.composer
        .queue
        .push_back(crate::vendor::Prompt::plain("later".into()));
    assert!(app.composer.editor.insert("draft"));
    let draft = app.composer.editor.layout(draft_width(38));
    let mut terminal = Terminal::new(TestBackend::new(38, 3)).unwrap();
    terminal
        .draw(|frame| {
            composer(frame, frame.area(), 38, &app, draft);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = (0..3)
        .flat_map(|y| (0..38).map(move |x| (x, y)))
        .map(|position| buffer[position].symbol())
        .collect();
    assert!(text.contains("draft"), "{text}");
    assert!(
        !text.contains("queued") && !text.contains("Enter send"),
        "{text}"
    );
}
mod model_detail_tests {
    use super::*;
    fn app() -> App {
        App::new(
            &octet_core::Config {
                model: Some("sonnet".into()),
                ..octet_core::Config::new(octet_core::Engine::CLAUDE, "claude", "/tmp")
            },
            "/tmp/journal".into(),
        )
    }
    #[test]
    fn full_model_details_survive_long_ids_and_reconnection_clears_metadata() {
        let mut a = app();
        let id = format!("provider-{}-version", "long".repeat(40));
        a.event(Event::Models(vec![octet_core::ModelInfo {
            selection: "sonnet".into(),
            id: Some(id.clone()),
            name: "Complete Provider Model Name".into(),
            description: "Provider details".into(),
        }]));
        assert!(a.model_details().contains("not yet reported"));
        a.event(Event::ModelSelected(id.clone()));
        a.show_models(1);
        let details = a
            .chat
            .entries
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(details.contains(&id));
        assert!(details.contains("Complete Provider Model Name"));
        assert!(details.contains("Requested selection: sonnet"));
        a.connection(
            &octet_core::Config::new(octet_core::Engine::CODEX, "codex", "/tmp"),
            "/tmp/new".into(),
        );
        assert!(a.conn.models.is_empty());
        assert!(a.conn.resolved_model.is_none());
        assert!(!a.model_details().contains(&id));
    }
    #[test]
    fn large_catalog_is_paged_without_evicting_current_details() {
        let mut a = app();
        a.event(Event::Models(
            (0..256)
                .map(|n| octet_core::ModelInfo {
                    selection: format!("model-{n}"),
                    id: Some(format!("full-id-{n}")),
                    name: format!("Name {n}"),
                    description: String::new(),
                })
                .collect(),
        ));
        a.show_models(13);
        let visible = a
            .visible_lines(72, 12)
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("claude 256 (live)"), "{visible}");
        assert!(visible.contains("Name 240"));
        assert!(a.chat.scroll > 0);
        assert!(
            a.chat
                .entries
                .iter()
                .any(|e| e.text.contains("full-id-255"))
        );
        assert!(
            a.chat
                .entries
                .iter()
                .any(|e| e.text.contains("Requested selection"))
        );
        assert!(
            !a.chat
                .entries
                .iter()
                .any(|e| e.text.contains("full-id-0\n"))
        );
        let count = a.chat.entries.len();
        a.show_models(usize::MAX);
        assert_eq!(a.chat.entries.len(), count + 1);
    }
    #[test]
    fn stopped_session_clears_pending_mode() {
        let c = octet_core::Config::new(octet_core::Engine::CLAUDE, "claude", "/tmp");
        let mut a = App::new(&c, "journal".into());
        a.conn.mode_pending = Some(octet_core::Mode::Auto);
        a.event(Event::ModeChanged(octet_core::Mode::Auto));
        assert_eq!(
            (a.conn.mode, a.conn.mode_pending),
            (octet_core::Mode::Auto, None)
        );
        a.conn.mode_pending = Some(octet_core::Mode::Ask);
        a.event(Event::Stopped);
        assert_eq!(
            (a.conn.mode, a.conn.mode_pending),
            (octet_core::Mode::Auto, None)
        );
    }
    #[test]
    fn header_shows_confirmed_and_pending_mode() {
        let c = octet_core::Config {
            mode: octet_core::Mode::Auto,
            ..octet_core::Config::new(octet_core::Engine::CODEX, "codex", "/tmp/project")
        };
        let mut a = App::new(&c, "journal".into());
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        let screen = |t: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            t.backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        };
        t.draw(|f| draw(f, &mut a)).unwrap();
        assert!(screen(&t).contains("auto"));
        assert_eq!(mode_chip(&a).style.fg, Some(ACCENT));
        a.conn.mode_pending = Some(octet_core::Mode::Ask);
        t.draw(|f| draw(f, &mut a)).unwrap();
        assert!(screen(&t).contains("ask…"));
        a.event(Event::ModeChanged(octet_core::Mode::FullAccess));
        assert_eq!(mode_chip(&a).style.fg, Some(AMBER));
        assert!(COMMANDS.iter().any(|spec| spec.name == "/mode"));
    }
}
#[test]
fn no_color_renders_without_colour_and_marks_the_selection() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = true;
    app.note("a note");
    app.overlay.palette = true;
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut reversed = 0;
    for y in 0..32 {
        for x in 0..100 {
            let cell = &buffer[(x, y)];
            assert_eq!(
                (cell.fg, cell.bg),
                (Color::Reset, Color::Reset),
                "({x}, {y})"
            );
            if cell.modifier.contains(Modifier::REVERSED) {
                reversed += 1;
            }
        }
    }
    assert!(reversed > 0, "the palette's selected row must still show");
}
#[test]
fn scrolling_up_keeps_the_view_full() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    // One entry of 13 lines: 15 rows with its blank line and label.
    let text: Vec<String> = (1..=13).map(|n| format!("line {n}")).collect();
    app.note(text.join("\n"));
    let others = app.chat.entries.len() - 1;
    assert_eq!(others, 0, "only the one entry");
    app.chat.scroll = 13;
    let shown = app.visible_lines(80, 10);
    assert_eq!(shown.len(), 10, "a short page at the top, blank below");
}
#[test]
fn approval_scroll_is_clamped() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(approval(1, "one short line"));
    app.overlay.approval_scroll = 500;
    let rows = screen(100, 30, &mut app);
    assert!(
        rows.iter().any(|row| row.contains("one short line")),
        "scrolled past its only line"
    );
    assert_eq!(app.overlay.approval_scroll, 0);
}
#[test]
fn the_approval_leads_with_action_command_folder_and_scope() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(crate::test_support::approval(
        3,
        "{\n  \"tool_name\": \"Bash\"\n}",
    ));
    let rows = screen(100, 30, &mut app);
    let at = |text: &str| {
        rows.iter()
            .position(|r| r.contains(text))
            .unwrap_or_else(|| panic!("{text}\n{}", rows.join("\n")))
    };
    assert!(at("Permission required · #3 · 1 pending") < at("Run a shell command"));
    assert!(at("Run a shell command") < at("$ cargo test"));
    assert!(at("$ cargo test") < at("in /tmp"));
    assert!(at("in /tmp") < at("Allows: This request only, once"));
    assert!(at("Allows:") < at("Full request · PgUp/PgDn"));
    assert!(at("Full request") < at("\"tool_name\": \"Bash\""));
    assert!(rows.iter().any(|r| r.contains("A  Allow once")));
}
#[test]
fn summary_lines_bound_commands_and_files() {
    let summary = octet_core::ApprovalSummary {
        command: Some("a\nb\nc\nd".into()),
        files: (0..9).map(|n| format!("f{n}")).collect(),
        ..octet_core::ApprovalSummary::new("Edit 9 files")
    };
    assert_eq!(
        summary_lines(&summary),
        [
            "Edit 9 files",
            "$ a",
            "  b",
            "  c",
            "  …",
            "f0",
            "f1",
            "f2",
            "f3",
            "f4",
            "+4 more",
            "Allows: This request only, once"
        ]
    );
}
#[test]
fn a_huge_approval_keeps_its_keys_on_a_small_screen() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(octet_core::Event::Approval {
        id: 1,
        detail: "x\n".repeat(500),
        summary: octet_core::ApprovalSummary {
            command: Some("y".repeat(5000)),
            files: (0..100).map(|n| format!("f{n}")).collect(),
            cwd: Some("/tmp".into()),
            ..octet_core::ApprovalSummary::new("Edit 100 files")
        },
    });
    for (w, h) in [(40, 12), (38, 12), (80, 24)] {
        let rows = screen(w, h, &mut app);
        assert!(
            rows.iter().any(|r| r.contains("Allow once")),
            "{w}×{h}\n{}",
            rows.join("\n")
        );
        for kept in [
            "Allows: This request only, once",
            "Edit 100 files",
            "in /tmp",
        ] {
            assert!(
                rows.iter().any(|r| r.contains(kept)),
                "{kept} at {w}×{h}\n{}",
                rows.join("\n")
            );
        }
    }
}
#[test]
fn fit_lead_cuts_the_middle_and_keeps_the_first_and_the_tail() {
    let rows: Vec<String> = (0..8).map(|n| format!("r{n}")).collect();
    assert_eq!(fit_lead(rows.clone(), 8, 2), rows, "it fits");
    assert_eq!(fit_lead(rows.clone(), 9, 2), rows, "room to spare");
    assert_eq!(fit_lead(rows.clone(), 5, 2), ["r0", "r1", "…", "r6", "r7"]);
    assert_eq!(fit_lead(rows.clone(), 4, 2), ["r0", "…", "r6", "r7"]);
    assert_eq!(
        fit_lead(rows.clone(), 3, 2),
        ["r0", "r6", "r7"],
        "no room for the mark"
    );
    assert_eq!(
        fit_lead(rows.clone(), 2, 2),
        ["r6", "r7"],
        "the tail outranks the first row"
    );
    assert_eq!(
        fit_lead(rows.clone(), 1, 2),
        ["r7"],
        "the last row outranks all"
    );
    assert!(fit_lead(rows, 0, 2).is_empty());
}
#[test]
fn help_rows_match_what_is_drawn() {
    let last = help_lines()
        .into_iter()
        .rev()
        .find(|line| !line.is_empty())
        .unwrap();
    let tail: String = last.chars().take(12).collect();
    for width in 40..100 {
        let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
        app.overlay.help = true;
        app.overlay.help_scroll = u16::MAX;
        let rows = screen(width, 14, &mut app);
        assert!(
            rows.iter().any(|row| row.contains(&tail)),
            "the help's last line is out of reach at width {width}"
        );
    }
}
#[test]
fn an_approval_hidden_behind_help_is_armed_only_once_seen() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(approval(1, "rm -rf build"));
    // Help covers the dialog while the approval arrives and waits.
    app.overlay.help = true;
    screen(100, 30, &mut app);
    std::thread::sleep(crate::app::APPROVAL_ARM);
    app.overlay.help = false;
    // Help just closed: the dialog is first seen now, so it is not armed.
    screen(100, 30, &mut app);
    assert!(!app.overlay.approval_armed(), "armed before it was seen");
    // Seen long enough, it arms; covered again and shown again, it waits anew.
    std::thread::sleep(crate::app::APPROVAL_ARM);
    assert!(app.overlay.approval_armed());
    app.overlay.palette = true;
    screen(100, 30, &mut app);
    app.overlay.palette = false;
    screen(100, 30, &mut app);
    assert!(
        !app.overlay.approval_armed(),
        "re-shown dialog was armed at once"
    );
}
#[test]
fn the_model_list_merges_every_provider() {
    let c = octet_core::Config::new(octet_core::Engine::CLAUDE, "claude", "/tmp");
    let mut a = App::new(&c, "journal".into());
    let model = |s: &str, name: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: Some(s.into()),
        name: name.into(),
        description: String::new(),
    };
    a.event(octet_core::Event::Models(vec![
        model("default", "Default"),
        model("opus", "Opus 5.5"),
    ]));
    a.models.catalogs.set(
        octet_core::Engine::CODEX,
        vec![model("gpt-6-astra", "GPT-6-Astra")],
        std::time::SystemTime::now() - std::time::Duration::from_hours(2),
    );
    a.show_models(1);
    let text = a.entries_text();
    assert!(
        text.contains("claude 2 (live) · codex 1 (cached 2 h ago)"),
        "{text}"
    );
    assert!(text.contains("Opus 5.5 · claude"), "{text}");
    assert!(text.contains("Select: /model opus"), "{text}");
    assert!(text.contains("GPT-6-Astra · codex"), "{text}");
    assert!(text.contains("Select: /model gpt-6-astra"), "{text}");
    // `default` names the current provider, so it is plain here.
    assert!(text.contains("Select: /model default"), "{text}");
    assert!(text.contains("switches provider when needed"), "{text}");
    assert!(!text.contains("starts fresh context"), "{text}");
    let claude = text.find("Opus 5.5 · claude").unwrap();
    let codex = text.find("GPT-6-Astra · codex").unwrap();
    assert!(claude < codex, "the current provider first");
}
#[test]
fn the_demo_lists_only_the_vendors() {
    let c = octet_core::Config::new(octet_core::Engine::DEMO, "demo", "/tmp");
    let mut a = App::new(&c, "journal".into());
    a.models.catalogs.set(
        octet_core::Engine::CODEX,
        vec![octet_core::ModelInfo {
            selection: "gpt-6-astra".into(),
            id: None,
            name: "GPT-6-Astra".into(),
            description: String::new(),
        }],
        std::time::SystemTime::now(),
    );
    a.show_models(1);
    let text = a.entries_text();
    assert!(!text.contains("demo 0"), "{text}");
    assert!(text.contains("codex 1 (cached just now)"), "{text}");
}
#[test]
fn the_popup_draws_items_cleaned() {
    let mut app = crate::test_support::app_for(octet_core::Engine::CODEX);
    app.composer.editor.set("/model ".into());
    app.composer.completion = Some(crate::composer::Completion {
        kind: crate::composer::Kind::Model,
        items: vec!["evil\u{202e}name · codex · Evil".into()],
        selected: 0,
        moved: false,
        start: 7,
    });
    let rows = screen(100, 30, &mut app).join("\n");
    assert!(rows.contains("evilname · codex"), "{rows}");
    assert!(!rows.contains('\u{202e}'));
}
#[test]
fn a_model_named_like_a_subcommand_gets_the_explicit_line() {
    let c = octet_core::Config::new(octet_core::Engine::CODEX, "codex", "/tmp");
    let mut a = App::new(&c, "journal".into());
    let model = |s: &str| octet_core::ModelInfo {
        selection: s.into(),
        id: None,
        name: s.into(),
        description: String::new(),
    };
    a.event(octet_core::Event::Models(vec![
        model("list"),
        model("refresh"),
    ]));
    a.show_models(1);
    let text = a.entries_text();
    assert!(text.contains("Select: /model codex list"), "{text}");
    assert!(text.contains("Select: /model codex refresh"), "{text}");
}
#[test]
fn a_palette_with_no_match_says_so() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.overlay.palette = true;
    app.overlay.palette_query = "qqqq".into();
    let rows = screen(100, 30, &mut app);
    assert!(rows.iter().any(|r| r.contains("No matching commands")));
    assert!(rows.iter().any(|r| r.contains("Commands · \"qqqq\"")));
}
#[test]
fn a_short_terminal_gets_a_one_row_header_and_no_vertical_margin() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = false;
    crate::test_support::idle(&mut app);
    app.event(Event::User("hi".into()));
    let rows = screen(100, 24, &mut app);
    assert!(rows[0].contains("Octet · ask"), "{}", rows[0]);
    assert!(rows[0].contains("● ready"), "{}", rows[0]);
    assert!(rows[0].contains("/tmp"), "{}", rows[0]);
    assert!(rows[1].contains("╭ Conversation"), "{}", rows[1]);
    // The demo is offline, so its standing hints say so.
    assert_eq!(rows[23].trim(), "Offline demo · F1 help · Ctrl+C ×2 quit");
    let transcript_rows = rows.iter().filter(|r| r.starts_with(" │")).count();
    assert!(
        transcript_rows >= 16,
        "{transcript_rows}\n{}",
        rows.join("\n")
    );
}
#[test]
fn a_tall_terminal_keeps_the_banner() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.monochrome = false;
    let rows = screen(100, 30, &mut app);
    let mini: String = rows[1].chars().skip(1).take(9).collect();
    assert_eq!(mini, "▀".repeat(9));
}
#[test]
fn a_narrow_short_header_puts_the_workspace_before_the_model() {
    let config = octet_core::Config {
        model: Some("a-very-long-model-name-indeed".into()),
        ..octet_core::Config::new(octet_core::Engine::CODEX, "codex", "/tmp/ws")
    };
    let mut app = App::new(&config, "journal".into());
    let rows = screen(60, 20, &mut app);
    let ws = rows[0].find("/tmp/ws").expect(&rows[0]);
    assert!(
        rows[0].find("a-very").is_none_or(|model| ws < model),
        "{}",
        rows[0]
    );
}
#[test]
fn a_compact_notice_stays_inside_the_side_margin() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.hint("hello");
    let rows = screen(100, 24, &mut app);
    assert!(rows[23].starts_with(' '), "{:?}", rows[23]);
    assert!(rows[23].contains("hello"), "{:?}", rows[23]);
}
#[test]
fn the_compact_status_shows_in_full_and_usage_only_when_it_fits() {
    for (width, usage_shows) in [(38, false), (60, false), (120, true)] {
        let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
        app.conn.usage = "$0.1234 session cost".into();
        app.event(approval(1, "{}"));
        let rows = screen(width, 20, &mut app);
        assert!(
            rows[0].contains("● approval needed"),
            "{width}: {}",
            rows[0]
        );
        assert_eq!(
            rows[0].contains("$0.1234 session cost"),
            usage_shows,
            "{width}: {}",
            rows[0]
        );
    }
}
fn stream_lines(app: &mut App, n: usize) {
    for i in 0..n {
        app.event(Event::Text(format!("line {i}\n")));
    }
}
#[test]
fn scrolled_up_the_view_stays_still_while_output_arrives() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    stream_lines(&mut app, 60);
    app.scroll_by(20);
    let before: Vec<String> = app
        .visible_lines(80, 10)
        .iter()
        .map(ToString::to_string)
        .collect();
    stream_lines(&mut app, 5);
    let after: Vec<String> = app
        .visible_lines(80, 10)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(before, after);
    assert!(app.chat.unseen);
    let rows = screen(100, 30, &mut app);
    assert!(
        rows.iter()
            .any(|r| r.contains("New output below · Ctrl+End follows")),
        "{}",
        rows.join("\n")
    );
    app.follow_latest();
    assert!(!app.chat.unseen);
    let rows = screen(100, 30, &mut app);
    assert!(!rows.iter().any(|r| r.contains("New output below")));
}
#[test]
fn resizing_while_scrolled_reanchors() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    stream_lines(&mut app, 60);
    app.scroll_by(20);
    let _ = app.visible_lines(80, 10);
    let scroll = app.chat.scroll;
    let _ = app.visible_lines(50, 10);
    assert_eq!(
        app.chat.scroll, scroll,
        "a new width re-anchors without moving"
    );
    stream_lines(&mut app, 2);
    let _ = app.visible_lines(50, 10);
    assert!(app.chat.scroll > scroll);
    // The anchor entry is trimmed away: no panic, still bounded.
    app.chat.entries.pop_front();
    let _ = app.visible_lines(50, 10);
}
#[test]
fn the_view_stays_still_across_a_reply_drain() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    // Full 100-byte lines (two rows each at 80 columns) until the reply nears its drain point.
    let drain_at = 64 * 1024 + 16 * 1024;
    let mut i = 0;
    while app.chat.entries.back().map_or(0, |e| e.text.len()) < drain_at - 160 {
        app.event(Event::Text(format!("{i:<98}\n")));
        i += 1;
    }
    // A last line that leaves 60 bytes before the drain point.
    let len = app.chat.entries.back().map_or(0, |e| e.text.len());
    app.event(Event::Text(format!(
        "{}\n",
        "w".repeat(drain_at - 60 - len - 1)
    )));
    app.scroll_by(20);
    let shown = |app: &mut App| -> Vec<String> {
        app.visible_lines(80, 10)
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    // A partial line, so that the event that crosses the drain adds no row.
    app.event(Event::Text("y".into()));
    let before = shown(&mut app);
    let len = app.chat.entries.back().map_or(0, |e| e.text.len());
    app.event(Event::Text("z".repeat(drain_at + 1 - len)));
    let after_drain = shown(&mut app);
    assert!(
        app.chat
            .entries
            .back()
            .is_some_and(|e| e.text.len() < drain_at),
        "the drain ran"
    );
    for n in 0..3 {
        app.event(Event::Text(format!("more {n}\n")));
        let _ = shown(&mut app);
    }
    let end = shown(&mut app);
    assert_eq!(before.len(), 10);
    assert_eq!(
        &before[..8],
        &after_drain[..8],
        "unchanged across the drain"
    );
    assert_eq!(&before[..8], &end[..8], "and after it");
    assert!(app.chat.unseen);
}
#[test]
fn trimming_the_anchor_entry_keeps_compensating() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Started);
    stream_lines(&mut app, 60);
    app.scroll_by(3);
    let _ = app.visible_lines(80, 10);
    // The anchor entry itself grows, then newer entries arrive after it.
    stream_lines(&mut app, 15);
    let _ = app.visible_lines(80, 10);
    app.note(
        (0..150)
            .map(|n| format!("note {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let _ = app.visible_lines(80, 10);
    // The anchor entry is trimmed away from the front.
    app.chat.entries.pop_front();
    let _ = app.visible_lines(80, 10);
    let scroll = app.chat.scroll;
    stream_lines(&mut app, 4);
    let _ = app.visible_lines(80, 10);
    assert!(app.chat.scroll > scroll, "{} > {scroll}", app.chat.scroll);
}
#[test]
fn scrolled_up_the_view_stays_still_as_running_calls_collapse() {
    let shown = |app: &mut App| -> Vec<String> {
        app.visible_lines(80, 10)
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    // The calls start below the reader, or were already the newest entry when they scrolled up.
    for before_scroll in [false, true] {
        let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
        app.event(Event::Started);
        stream_lines(&mut app, 60);
        let reads = |app: &mut App, phase| {
            for n in 1..=4 {
                app.event(tool(&n.to_string(), "Read", &format!("f{n}.rs"), phase));
            }
        };
        if before_scroll {
            reads(&mut app, ToolPhase::Running);
        }
        app.scroll_by(20);
        let _ = shown(&mut app);
        if !before_scroll {
            reads(&mut app, ToolPhase::Running);
        }
        let before = shown(&mut app);
        reads(&mut app, ToolPhase::Done);
        assert_eq!(shown(&mut app), before, "before scroll: {before_scroll}");
    }
}

#[test]
fn a_long_approval_keeps_the_full_request_reachable() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.event(Event::Approval {
        id: 1,
        detail: "RAW_FULL_REQUEST_MARKER".into(),
        summary: octet_core::ApprovalSummary {
            command: Some(format!("echo {}", "long_argument ".repeat(160))),
            cwd: Some("/work".into()),
            ..octet_core::ApprovalSummary::new("Run a shell command")
        },
    });
    let mut reachable = false;
    for scroll in 0..100 {
        app.overlay.approval_scroll = scroll;
        let text = screen(80, 24, &mut app).join("\n");
        reachable |= text.contains("RAW_FULL_REQUEST_MARKER");
    }
    assert!(
        reachable,
        "full request is inaccessible at every scroll position"
    );
}

#[test]
fn a_late_tool_completion_below_the_view_keeps_scrollback_still() {
    let mut app = crate::test_support::app_for(octet_core::Engine::DEMO);
    app.note(
        (0..40)
            .map(|i| format!("history line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    app.event(Event::Started);
    app.event(tool("1", "Bash", "cargo test", ToolPhase::Running));
    app.note("hook notice after tool start");
    app.chat.scroll = 12;
    let before = app.visible_lines(80, 6);
    app.event(Event::Tool(octet_core::ToolCall {
        id: "1".into(),
        name: "Bash".into(),
        target: "cargo test".into(),
        phase: ToolPhase::Failed,
        detail: (0..8)
            .map(|i| format!("error line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }));
    let after = app.visible_lines(80, 6);
    assert_eq!(
        before, after,
        "tool completion below viewport moved the history"
    );
}
