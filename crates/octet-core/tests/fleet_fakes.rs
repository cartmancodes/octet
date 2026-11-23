//! The scripted fake vendors for fleet runs: `protocol-child`, with
//! `OCTET_FLEET_SCRIPT` naming a script, performs a turn whose prompt starts
//! with a fleet header, in both the Codex and the Claude dialect.
//!
//! The script is named by the test process's environment, which the vendor
//! inherits. Every test here holds `ENV` for its whole run, so the variable
//! changes only while no other test in this binary runs.
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unwrap that fails is the test failing"
)]
use octet_core::{Command, Config, Engine, Event, Session};
use octet_testkit::{TempDir, scenario::FLEET_SCRIPT_ENV};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tokio::time::timeout;

/// Serializes the tests: they share the process environment.
static ENV: Mutex<()> = Mutex::new(());

/// The header every fleet prompt starts with, for `step` and `round`.
fn prompt(step: &str, round: u32) -> String {
    format!("[octet-fleet step={step} round={round} attempt=1 candidate=c1 tree=t1]\nDo the step.")
}

/// A scripted run: the script's files in `dir`, the workspace the vendor
/// works in, and the engine to drive.
struct Run {
    dir: TempDir,
    work: PathBuf,
    engine: Engine,
}

impl Run {
    /// A run of `engine` with `turns` as the script's turns and a startup marker.
    fn new(engine: Engine, turns: &Value) -> Self {
        let dir = TempDir::new("octet-fleet-fakes");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let script = json!({
            "log": dir.path().join("log"),
            "startup_marker": dir.path().join("marker"),
            "turns": turns,
        });
        std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
        Self { dir, work, engine }
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn marker(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("marker")).unwrap_or_default()
    }

    /// Runs `body` with the script in force, on its own runtime.
    fn with<T>(&self, body: impl AsyncFnOnce(Opener<'_>) -> T) -> T {
        let _held = ENV
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: every test in this binary holds `ENV`, and the runtime that
        // starts the vendor is built after this call and gone before it returns.
        unsafe { std::env::set_var(FLEET_SCRIPT_ENV, self.dir.path().join("script.json")) };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let out = runtime.block_on(body(Opener(self)));
        runtime.shutdown_timeout(Duration::from_secs(5));
        // SAFETY: as above; no runtime is left running.
        unsafe { std::env::remove_var(FLEET_SCRIPT_ENV) };
        out
    }
}

/// Opens sessions of a [`Run`].
struct Opener<'a>(&'a Run);

impl Opener<'_> {
    /// A session on the fake, resuming `resume`, and its `Ready` session ID.
    async fn open(&self, resume: Option<&str>) -> (Session, String) {
        let mut config = Config::new(self.0.engine, octet_testkit::protocol_child(), &self.0.work);
        config.resume = resume.map(str::to_owned);
        let mut session = Session::open(config, self.0.dir.path().join("journal"))
            .await
            .unwrap();
        loop {
            match next(&mut session).await {
                Event::Ready { session: id } => return (session, id),
                Event::Error(error) => panic!("{error}"),
                Event::Stopped => panic!("stopped before it was ready"),
                _ => {}
            }
        }
    }
}

async fn next(session: &mut Session) -> Event {
    timeout(Duration::from_secs(10), session.events.recv())
        .await
        .expect("no event in time")
        .expect("the session ended")
}

/// Sends `text` and returns the reply text up to the turn's end.
async fn turn(session: &mut Session, text: &str) -> String {
    session.handle.send(Command::Prompt(text.into())).unwrap();
    let mut reply = String::new();
    loop {
        match next(session).await {
            Event::Text(chunk) => reply.push_str(&chunk),
            Event::Finished { .. } => return reply,
            Event::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// Waits until the log holds `lines` lines, without blocking the runtime the
/// session runs on.
async fn log_reaches(run: &Run, lines: usize) {
    timeout(Duration::from_secs(10), async {
        while run.log().len() < lines {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the vendor never logged the turn");
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().display().to_string()
}

/// A turn that writes, deletes and replies, run through `engine`, then
/// resumed.
fn writes_and_replies(engine: Engine) {
    let run = Run::new(
        engine,
        &json!({
            "write#1": {"write": {"out/deep/a.txt": "hello"}, "reply": "scripted reply"},
            "other#2": {"delete": ["out/deep/a.txt"], "reply": "deleted"},
        }),
    );
    std::fs::create_dir_all(run.work.join("out/deep")).unwrap();
    run.with(async |open| {
        let (mut session, id) = open.open(None).await;
        assert_eq!(run.marker().lines().count(), 1, "the marker precedes Ready");
        let sent = prompt("write", 1);
        assert_eq!(turn(&mut session, &sent).await, "scripted reply");
        assert_eq!(
            std::fs::read_to_string(run.work.join("out/deep/a.txt")).unwrap(),
            "hello"
        );
        let log = run.log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0]["key"], "write#1");
        assert_eq!(log[0]["resumed"], false);
        assert_eq!(log[0]["prompt"], sent);
        assert_eq!(log[0]["cwd"], canonical(&run.work));
        assert!(log[0].get("missing").is_none());
        session.shutdown().await;

        let id = if id.is_empty() { "fixture".into() } else { id };
        let (mut session, _) = open.open(Some(&id)).await;
        assert_eq!(run.marker().lines().count(), 2, "one marker line per start");
        assert_eq!(turn(&mut session, &prompt("other", 2)).await, "deleted");
        assert!(!run.work.join("out/deep/a.txt").exists());
        let log = run.log();
        assert_eq!(log[1]["key"], "other#2");
        assert_eq!(log[1]["resumed"], true);
        session.shutdown().await;
    });
}

#[test]
fn a_codex_turn_writes_replies_and_a_resumed_one_says_so() {
    writes_and_replies(Engine::CODEX);
}

#[test]
fn a_claude_turn_writes_replies_and_a_resumed_one_says_so() {
    writes_and_replies(Engine::CLAUDE);
}

/// A turn with no script fails the test by its reply, and a repeat of a key
/// plays the same turn.
fn missing_and_repeated(engine: Engine) {
    let run = Run::new(engine, &json!({"s#1": {"reply": "same"}}));
    run.with(async |open| {
        let (mut session, _) = open.open(None).await;
        assert_eq!(turn(&mut session, &prompt("s", 1)).await, "same");
        assert_eq!(turn(&mut session, &prompt("s", 1)).await, "same");
        assert_eq!(
            turn(&mut session, &prompt("nope", 7)).await,
            "fleet-fake: no turn nope#7"
        );
        let log = run.log();
        assert_eq!(log.len(), 3);
        assert!(log[0].get("missing").is_none());
        assert_eq!(log[2]["key"], "nope#7");
        assert_eq!(log[2]["missing"], true);
        session.shutdown().await;
    });
}

#[test]
fn codex_repeats_a_turn_and_reports_a_missing_one() {
    missing_and_repeated(Engine::CODEX);
}

#[test]
fn claude_repeats_a_turn_and_reports_a_missing_one() {
    missing_and_repeated(Engine::CLAUDE);
}

/// A prompt that is not a fleet header runs the ordinary scenarios even with
/// a script in force.
fn ordinary_prompts_are_untouched(engine: Engine, reply: &str) {
    let run = Run::new(engine, &json!({}));
    run.with(async |open| {
        let (mut session, _) = open.open(None).await;
        assert_eq!(turn(&mut session, "just a prompt").await, reply);
        assert!(run.log().is_empty());
        session.shutdown().await;
    });
}

#[test]
fn ordinary_prompts_are_untouched_by_a_script() {
    ordinary_prompts_are_untouched(Engine::CODEX, octet_testkit::scenario::CODEX_REPLY);
    ordinary_prompts_are_untouched(Engine::CLAUDE, octet_testkit::scenario::CLAUDE_REPLY);
}

/// A hold ends only when the session interrupts; `before_write` never
/// writes, `after_write` has written by then.
fn holds(engine: Engine) {
    let run = Run::new(
        engine,
        &json!({
            "a#1": {"write": {"before.txt": "x"}, "hold": "before_write"},
            "b#1": {"write": {"after.txt": "x"}, "hold": "after_write"},
        }),
    );
    run.with(async |open| {
        let (mut session, _) = open.open(None).await;
        for (n, step) in ["a", "b"].into_iter().enumerate() {
            session
                .handle
                .send(Command::Prompt(prompt(step, 1)))
                .unwrap();
            log_reaches(&run, n + 1).await;
            // Still held a moment later: no turn has ended.
            tokio::time::sleep(Duration::from_millis(300)).await;
            while let Ok(event) = session.events.try_recv() {
                assert!(!matches!(event, Event::Finished { .. }), "{event:?}");
            }
            session.handle.interrupt();
            loop {
                if matches!(next(&mut session).await, Event::Finished { .. }) {
                    break;
                }
            }
        }
        assert!(!run.work.join("before.txt").exists());
        assert!(run.work.join("after.txt").exists());
        session.shutdown().await;
    });
}

#[test]
fn codex_holds_before_and_after_writing() {
    holds(Engine::CODEX);
}

#[test]
fn claude_holds_before_and_after_writing() {
    holds(Engine::CLAUDE);
}

/// `exit_after` ends the vendor right after the turn's reply.
fn exits_after(engine: Engine) {
    let run = Run::new(
        engine,
        &json!({"x#1": {"reply": "bye", "exit_after": true}}),
    );
    run.with(async |open| {
        let (mut session, _) = open.open(None).await;
        assert_eq!(turn(&mut session, &prompt("x", 1)).await, "bye");
        loop {
            match next(&mut session).await {
                Event::Stopped | Event::Error(_) => break,
                _ => {}
            }
        }
        session.shutdown().await;
    });
}

#[test]
fn codex_exits_after_the_turn() {
    exits_after(Engine::CODEX);
}

#[test]
fn claude_exits_after_the_turn() {
    exits_after(Engine::CLAUDE);
}
