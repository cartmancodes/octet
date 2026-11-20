//! `octet pair` against a real hub, run in this process in a temporary directory with a relay in
//! process and a test phone, and against a scripted control socket where the clock is paused.
//! Nothing here installs a service: the hub is started by the test's own `start` step.
use super::*;
use crate::args::{CliError, Parsed, parse_args};
use octet_hub::Hub;
use octet_testkit::{
    TempDir,
    relay::{Held, Link, Relay, hold_with, roomy},
};
use std::{
    ffi::OsString,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{DuplexStream, duplex},
    sync::oneshot,
    task::JoinHandle,
};

/// How long any one step may take before the test fails.
const WAIT: Duration = Duration::from_secs(20);

/// What `octet pair` wrote, shared with the test while it runs.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Shared {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `start` step that must not run: the hub is running already.
fn never() -> impl FnOnce() -> std::future::Ready<Result<(), CliError>> {
    || std::future::ready(Err(CliError::Run("the hub was started again".into())))
}

/// A data directory, a relay in process, and the hub once started, serving the directory and
/// dialling that relay.
struct Rig {
    dir: TempDir,
    relay: Relay,
    stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl Rig {
    async fn new() -> Self {
        let dir = TempDir::new("opair");
        std::fs::create_dir_all(dir.path()).unwrap();
        Self {
            dir,
            relay: Relay::start_with(roomy()).await,
            stop: Arc::default(),
        }
    }

    /// A rig whose hub is answering.
    async fn running() -> Self {
        let rig = Self::new().await;
        (rig.start())().await.unwrap();
        timeout(WAIT, async {
            while !answers(&Socket::of(rig.dir.path())).await {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub starts");
        rig
    }

    /// A `start` step that serves the hub in this process, as the service would.
    fn start(&self) -> impl FnOnce() -> std::future::Ready<Result<(), CliError>> + use<> {
        let (directory, relay, stop) = (
            self.dir.path().to_owned(),
            self.relay.url(),
            Arc::clone(&self.stop),
        );
        move || {
            let (sender, stopped) = oneshot::channel::<()>();
            *stop.lock().unwrap() = Some(sender);
            tokio::spawn(async move {
                let shutdown = async {
                    let _ = stopped.await;
                };
                let log = Arc::new(|_: &str| {});
                Hub::serve_with(directory, relay, log, shutdown, oneshot::channel().0).await
            });
            std::future::ready(Ok(()))
        }
    }

    /// Runs `octet pair` here, with tmux, a wide terminal and `start`.
    fn pair<S, F>(&self, start: S) -> Run
    where
        S: FnOnce() -> F + Send + 'static,
        F: Future<Output = Result<(), CliError>> + Send + 'static,
    {
        Run::spawn(
            self.dir.path(),
            World {
                control: Socket::of(self.dir.path()),
                start,
                tmux: true,
                width: None,
            },
        )
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.lock().unwrap().take() {
            let _ = stop.send(());
        }
    }
}

/// `octet pair` running in a task: what it wrote, and the owner's end of its input.
struct Run {
    output: Shared,
    owner: Option<DuplexStream>,
    task: JoinHandle<Result<i32, CliError>>,
}

impl Run {
    fn spawn<S, F>(directory: &Path, world: World<Socket, S>) -> Self
    where
        S: FnOnce() -> F + Send + 'static,
        F: Future<Output = Result<(), CliError>> + Send + 'static,
    {
        let output = Shared::default();
        let (owner, input) = duplex(64);
        let (directory, written) = (directory.to_owned(), output.clone());
        let task = tokio::spawn(async move {
            pair_with(&directory, BufReader::new(input), written, world).await
        });
        Self {
            output,
            owner: Some(owner),
            task,
        }
    }

    /// The output once it holds `text`.
    async fn wait_for(&self, text: &str) -> String {
        timeout(WAIT, async {
            loop {
                let written = self.output.text();
                if written.contains(text) {
                    return written;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never wrote {text:?}; wrote:\n{}", self.output.text()))
    }

    /// The pairing link it shows.
    async fn link(&self) -> Link {
        let written = self.wait_for("octet://pair/").await;
        let line = written
            .lines()
            .find(|line| line.starts_with("octet://pair/"))
            .unwrap();
        Link::parse(line).unwrap()
    }

    /// The owner types `text`.
    async fn type_in(&mut self, text: &str) {
        self.owner
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .await
            .unwrap();
    }

    /// The owner's input ends.
    fn end_input(&mut self) {
        self.owner = None;
    }

    /// Its exit code and everything it wrote, once it ends.
    async fn finish(self) -> (i32, String) {
        let code = timeout(WAIT, self.task)
            .await
            .expect("octet pair ends")
            .unwrap()
            .unwrap();
        (code, self.output.text())
    }
}

/// The prompt for a phone called `name` that `held` the code it was sent.
fn prompt(held: &Held, verb: &str, name: &str) -> String {
    let code = held.code["code"].as_str().unwrap();
    format!(
        "Phone shows {} {}? {verb} \"{name}\" (ios)? [y/N]",
        &code[..3],
        &code[3..]
    )
}

/// The names of the devices paired in `directory`.
async fn device_names(directory: &Path) -> Vec<String> {
    let store = Store::open_devices(directory).await.unwrap();
    store.devices().iter().map(|d| d.name.clone()).collect()
}

fn parse(list: &[&str]) -> Result<Parsed, CliError> {
    parse_args(list.iter().map(OsString::from))
}

#[test]
fn pair_parses() {
    assert_eq!(parse(&["pair"]), Ok(Parsed::Pair { journal_dir: None }));
    assert_eq!(
        parse(&["pair", "--journal-dir", "/d"]),
        Ok(Parsed::Pair {
            journal_dir: Some(PathBuf::from("/d"))
        })
    );
    assert_eq!(parse(&["pair", "--help"]), Ok(Parsed::PairHelp));
    assert_eq!(parse(&["pair", "-h"]), Ok(Parsed::PairHelp));
    assert_eq!(
        parse(&["pair", "now"]),
        Err(CliError::Usage(
            "Unknown argument now. Use pair --help.".into()
        ))
    );
    assert_eq!(
        parse(&["pair", "--journal-dir"]),
        Err(CliError::Usage(
            "--journal-dir requires a value. Use --help.".into()
        ))
    );
    // Only the first argument makes it the pair command.
    assert!(matches!(
        parse(&["--no-remote", "pair"]),
        Err(CliError::Usage(_))
    ));
}

#[test]
fn port_and_owner_are_no_longer_flags() {
    for (list, message) in [
        (
            &["pair", "--port", "47823"][..],
            "Unknown argument --port. Use pair --help.",
        ),
        (
            &["pair", "--owner", "me@example.com"][..],
            "Unknown argument --owner. Use pair --help.",
        ),
        (
            &["hub", "--port", "47823"][..],
            "Unknown argument --port. Use hub --help.",
        ),
        (
            &["hub", "--owner", "me@example.com"][..],
            "Unknown argument --owner. Use hub --help.",
        ),
        (
            &["--port", "47823"][..],
            "Unknown option --port. Use --help.",
        ),
        (
            &["--owner", "me@example.com"][..],
            "Unknown option --owner. Use --help.",
        ),
    ] {
        assert_eq!(
            parse(list),
            Err(CliError::Usage(message.into())),
            "{list:?}"
        );
    }
}

#[tokio::test]
async fn pair_starts_the_hub_when_it_is_not_running() {
    let rig = Rig::new().await;
    let starts = Arc::new(AtomicUsize::new(0));
    let start = {
        let (serve, starts) = (rig.start(), Arc::clone(&starts));
        move || {
            starts.fetch_add(1, Ordering::SeqCst);
            serve()
        }
    };
    let mut run = rig.pair(start);
    let link = run.link().await;
    assert_eq!(starts.load(Ordering::SeqCst), 1, "the hub was started once");
    let held = hold_with(&link, "Pixel").await;
    run.wait_for(&prompt(&held, "Pair", "Pixel")).await;
    run.type_in("y\n").await;
    let (code, written) = run.finish().await;
    assert_eq!(code, 0, "{written}");
    assert!(written.starts_with("Starting the hub\n"), "{written}");
    assert!(written.contains("Paired \"Pixel\""), "{written}");
    assert_eq!(device_names(rig.dir.path()).await, ["Pixel"]);
    drop(held);
}

#[tokio::test]
async fn yes_pairs_and_no_does_not() {
    let rig = Rig::running().await;
    let answers = [
        (Some("y\n"), true),
        (Some("Y\n"), true),
        (Some("yes\n"), true),
        (Some("\n"), false),
        (Some("n\n"), false),
        (None, false),
    ];
    let mut paired = Vec::new();
    for (n, (typed, pairs)) in answers.into_iter().enumerate() {
        let name = format!("Phone {n}");
        let mut run = rig.pair(never());
        let link = run.link().await;
        let held = hold_with(&link, &name).await;
        run.wait_for(&prompt(&held, "Pair", &name)).await;
        match typed {
            Some(text) => run.type_in(text).await,
            None => run.end_input(),
        }
        let (code, written) = run.finish().await;
        let said = if pairs {
            format!("Paired \"{name}\"")
        } else {
            NOT_PAIRED.to_owned()
        };
        assert!(
            written.ends_with(&format!("{said}\n")),
            "{typed:?}: {written}"
        );
        assert_eq!(code, i32::from(!pairs), "{typed:?}: {written}");
        if pairs {
            paired.push(name);
        }
        assert_eq!(device_names(rig.dir.path()).await, paired, "{typed:?}");
        drop(held);
    }
}

/// A control socket that answers from a script: `pair_secret` with `link`, `pair_wait` with
/// `question`, and `pair_answer` with `answer`, keeping every request.
#[derive(Clone)]
struct Scripted {
    link: String,
    question: Value,
    answer: Value,
    asked: Arc<Mutex<Vec<Value>>>,
}

impl Scripted {
    fn new(question: Value, answer: Value) -> Self {
        Self {
            link: format!("octet://pair/{}", "A".repeat(140)),
            question,
            answer,
            asked: Arc::default(),
        }
    }

    /// The `type` of every request so far.
    fn kinds(&self) -> Vec<String> {
        let asked = self.asked.lock().unwrap();
        asked
            .iter()
            .map(|request| request["type"].as_str().unwrap().to_owned())
            .collect()
    }
}

impl Control for Scripted {
    async fn call(&self, request: &Value) -> Option<Value> {
        // Answered after a turn of the runtime, as a socket would be.
        tokio::task::yield_now().await;
        self.asked.lock().unwrap().push(request.clone());
        Some(match request["type"].as_str()? {
            "status" => json!({"relay": "registered", "devices": 0}),
            "pair_secret" => json!({"link": self.link}),
            "pair_wait" => self.question.clone(),
            "pair_answer" => self.answer.clone(),
            _ => json!({"error": "Unknown request"}),
        })
    }
}

/// The hub's question about a phone called `name`.
fn question(name: &str, replaces: bool) -> Value {
    json!({"pending": 7, "code": "482913", "name": name, "platform": "ios", "replaces": replaces})
}

/// An input the owner never types in, and the end that keeps it open.
fn silent() -> (DuplexStream, BufReader<DuplexStream>) {
    let (owner, input) = duplex(64);
    (owner, BufReader::new(input))
}

#[tokio::test(start_paused = true)]
async fn the_prompt_times_out_with_pairing_timed_out() {
    let dir = TempDir::new("opair");
    std::fs::create_dir_all(dir.path()).unwrap();
    // The hub would say it refused the phone; the command's own clock says why.
    let control = Scripted::new(question("Pixel", false), json!({"paired": false}));
    let (_owner, input) = silent();
    let mut written = Vec::new();
    let began = Instant::now();
    let world = World {
        control: control.clone(),
        start: never(),
        tmux: true,
        width: None,
    };
    let code = pair_with(dir.path(), input, &mut written, world)
        .await
        .unwrap();
    let waited = began.elapsed();
    let written = String::from_utf8(written).unwrap();
    assert_eq!(code, 1, "{written}");
    assert!(
        written.ends_with("Phone shows 482 913? Pair \"Pixel\" (ios)? [y/N] \nPairing timed out\n"),
        "{written}"
    );
    assert!(!written.contains(NOT_PAIRED), "{written}");
    assert!(
        (ANSWER_WITHIN..ANSWER_WITHIN + POLL_EVERY * 2).contains(&waited),
        "{waited:?}"
    );
    // The phone was asked after about once a second, and refused at the end.
    let kinds = control.kinds();
    let polls = kinds.iter().filter(|kind| *kind == "pair_wait").count();
    assert!((100..=121).contains(&polls), "{polls} asks");
    assert_eq!(kinds.last().map(String::as_str), Some("pair_answer"));
    let last = control.asked.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["accept"], false);
    assert_eq!(last["pending"], 7);
}

#[tokio::test]
async fn a_pairing_that_replaces_asks_replace() {
    let rig = Rig::running().await;
    for (typed, verb) in [("y\n", "Pair"), ("n\n", "Replace")] {
        let mut run = rig.pair(never());
        let link = run.link().await;
        let held = hold_with(&link, "Pixel").await;
        run.wait_for(&prompt(&held, verb, "Pixel")).await;
        run.type_in(typed).await;
        let (_, written) = run.finish().await;
        let other = if verb == "Pair" { "Replace" } else { "Pair" };
        assert!(
            !written.contains(&prompt(&held, other, "Pixel")),
            "{written}"
        );
        drop(held);
    }
    // No replaced it: the first stays.
    assert_eq!(device_names(rig.dir.path()).await, ["Pixel"]);
}

#[tokio::test]
async fn a_phone_that_leaves_ends_the_prompt() {
    let rig = Rig::running().await;
    let run = rig.pair(never());
    let link = run.link().await;
    let mut held = hold_with(&link, "Pixel").await;
    run.wait_for(&prompt(&held, "Pair", "Pixel")).await;
    held.phone.close().await;
    // The owner never answers: the prompt ends because the phone went.
    let (code, written) = run.finish().await;
    assert_eq!(code, 1, "{written}");
    assert!(written.ends_with(&format!("\n{PHONE_LEFT}\n")), "{written}");
    assert!(device_names(rig.dir.path()).await.is_empty());
}

#[tokio::test]
async fn a_missing_tmux_is_said_once() {
    for tmux in [false, true] {
        let dir = TempDir::new("opair");
        std::fs::create_dir_all(dir.path()).unwrap();
        let control = Scripted::new(question("Pixel", false), json!({"paired": false}));
        let world = World {
            control,
            start: never(),
            tmux,
            width: None,
        };
        let mut written = Vec::new();
        let code = pair_with(dir.path(), &b"n\n"[..], &mut written, world)
            .await
            .unwrap();
        let written = String::from_utf8(written).unwrap();
        assert_eq!(code, 1, "{written}");
        let said = written.matches(NO_TMUX).count();
        assert_eq!(said, usize::from(!tmux), "{written}");
        assert_eq!(
            NO_TMUX,
            "tmux isn't installed: the phone can watch and answer, but can't start sessions"
        );
    }
}

#[tokio::test]
async fn a_narrow_terminal_gets_the_link_and_no_qr() {
    let dir = TempDir::new("opair");
    std::fs::create_dir_all(dir.path()).unwrap();
    let expired = Scripted::new(json!({"expired": true}), json!({}));
    let link = expired.link.clone();
    let width = octet_tui::qr_rows(&link).unwrap()[0].chars().count();
    let directory = dir.path().to_owned();
    let run = |width| {
        let directory = directory.clone();
        let world = World {
            control: expired.clone(),
            start: never(),
            tmux: true,
            width: Some(width),
        };
        async move {
            let mut written = Vec::new();
            let code = pair_with(&directory, &b""[..], &mut written, world)
                .await
                .unwrap();
            (code, String::from_utf8(written).unwrap())
        }
    };
    let (code, narrow) = run(width - 1).await;
    assert_eq!(code, 1);
    assert!(narrow.contains(WIDEN), "{narrow}");
    assert!(narrow.lines().any(|line| line == link), "{narrow}");
    assert!(!narrow.contains('█') && !narrow.contains('▀'), "{narrow}");
    assert!(narrow.ends_with(&format!("{LINK_EXPIRED}\n")), "{narrow}");
    let (_, wide) = run(width).await;
    assert!(!wide.contains(WIDEN), "{wide}");
    assert!(wide.contains('█') || wide.contains('▀'), "{wide}");
    assert!(wide.lines().any(|line| line == link), "{wide}");
}

#[tokio::test]
async fn devices_on_another_relay_are_counted() {
    let rig = Rig::running().await;
    // Two phones paired through a relay the hub no longer uses, and one through its own.
    let mut store = Store::open(rig.dir.path()).await.unwrap();
    let now = std::time::SystemTime::now();
    for (name, key, relay) in [
        ("Old 1", "ab", "wss://old.example".to_owned()),
        ("Old 2", "cd", "wss://old.example:8443".to_owned()),
        ("Here", "ef", rig.relay.url()),
    ] {
        let secret = store.pairing_secret(now);
        store
            .pair(&secret, name, "ios", &key.repeat(32), &relay, now)
            .await
            .unwrap();
    }
    let mut run = rig.pair(never());
    let written = run.wait_for("Waiting for a phone").await;
    assert!(
        written.contains("\n2 phones are paired through another relay and must pair again\n"),
        "{written}"
    );
    run.end_input();
    run.task.abort();
    // The link names the relay the hub dials, as each device records the one it paired through.
    let link = written
        .lines()
        .find(|line| line.starts_with("octet://pair/"))
        .unwrap();
    assert_eq!(link_relay(link), Some(rig.relay.url()));
}

#[test]
fn a_link_names_its_relay() {
    let mut payload = vec![1_u8];
    payload.extend([7_u8; 96]);
    payload.extend(b"relay.example:8443");
    let encoded = base64url(&payload);
    assert_eq!(
        link_relay(&format!("octet://pair/{encoded}")).as_deref(),
        Some("wss://relay.example:8443")
    );
    payload[0] = 0;
    let plain = base64url(&payload);
    assert_eq!(
        link_relay(&format!("octet://pair/{plain}")).as_deref(),
        Some("ws://relay.example:8443")
    );
    assert_eq!(link_relay("octet://pair/!!"), None);
    assert_eq!(link_relay(&format!("https://x/{encoded}")), None);
}

/// Unpadded base64url of `bytes`, apart from the code under test.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(
                ALPHABET[usize::try_from(n >> (18 - 6 * i) & 63).unwrap()],
            ));
        }
    }
    out
}
