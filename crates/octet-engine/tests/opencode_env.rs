//! What opencode sessions say about the user's `OPENCODE_CONFIG_CONTENT`,
//! which Octet reads from its own environment. This is its own test binary
//! with one test, so changing that variable cannot race another test.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]
use octet_engine::live::{Command, Config, Engine, Event, Mode, spawn};
use octet_testkit::scenario;
use std::{ffi::OsString, time::Duration};
use tokio::time::timeout;

/// The session's next event, failing instead of hanging.
async fn next(events: &mut tokio::sync::mpsc::Receiver<Event>) -> Event {
    timeout(Duration::from_secs(10), events.recv())
        .await
        .unwrap()
        .unwrap()
}

/// The notices an opencode session on the fake gives before it is ready,
/// and the `OPENCODE_CONFIG_CONTENT` its process was started with, while
/// Octet's own environment holds `content`.
fn session_with(content: OsString) -> (Vec<String>, serde_json::Value) {
    // SAFETY: this binary has one test, and no thread that reads the
    // environment is running: each session's runtime is built after this
    // call and shut down before the next one.
    unsafe { std::env::set_var("OPENCODE_CONFIG_CONTENT", content) };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let seen = runtime.block_on(async {
        let config = Config::new(
            Engine::OPENCODE,
            octet_testkit::protocol_child(),
            std::env::temp_dir(),
        );
        let (handle, mut events, task) = spawn(config);
        let mut notices = Vec::new();
        loop {
            match next(&mut events).await {
                Event::Notice(notice) => notices.push(notice),
                Event::ModeChanged(Mode::Ask) => break,
                Event::Error(error) => panic!("{error}"),
                Event::Stopped => panic!("stopped before it was ready"),
                _ => {}
            }
        }
        handle
            .send(Command::Prompt(scenario::PARAMS.into()))
            .unwrap();
        let mut text = String::new();
        loop {
            match next(&mut events).await {
                Event::Text(t) => text.push_str(&t),
                Event::Finished { .. } => break,
                _ => {}
            }
        }
        handle.shutdown();
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        let echo: serde_json::Value = serde_json::from_str(&text).unwrap();
        let sent = echo["env"]["OPENCODE_CONFIG_CONTENT"].as_str().unwrap();
        (notices, serde_json::from_str(sent).unwrap())
    });
    runtime.shutdown_timeout(Duration::from_secs(5));
    seen
}

#[test]
fn opencode_says_when_the_users_content_is_replaced_or_ignored() {
    use std::os::unix::ffi::OsStringExt;
    let agents = |content: &serde_json::Value| {
        let mut names: Vec<String> = content["agent"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    };
    let octet = ["octet-accept-edits", "octet-ask", "octet-full-access"];

    let (notices, content) = session_with("[1]".into());
    assert_eq!(
        notices,
        ["OPENCODE_CONFIG_CONTENT is not a JSON object; Octet replaced it for this session"]
    );
    assert_eq!(agents(&content), octet);

    let (notices, content) = session_with(r#"{"model":"x","agent":5}"#.into());
    assert_eq!(
        notices,
        [
            "OPENCODE_CONFIG_CONTENT's agent is not a JSON object; Octet replaced it for this session"
        ]
    );
    assert_eq!(content["model"], "x");
    assert_eq!(agents(&content), octet);

    let (notices, content) = session_with(OsString::from_vec(vec![b'{', 0xff, b'}']));
    assert_eq!(
        notices,
        ["OPENCODE_CONFIG_CONTENT is not valid UTF-8; Octet ignored it for this session"]
    );
    assert_eq!(agents(&content), octet);

    // Content Octet can use gives no notice.
    let (notices, content) = session_with(r#"{"model":"y"}"#.into());
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(content["model"], "y");
}
