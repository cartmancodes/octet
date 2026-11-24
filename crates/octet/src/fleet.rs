//! `octet fleet`: the headless front end of the workflow engine. It resolves
//! and validates a workflow, asks whether to trust it, starts, resumes or
//! stops a run, shows the run's events as text or JSON lines, and answers
//! what the run cannot decide itself (write steps' approvals, a pause's
//! decision) on a terminal.
//!
//! Exit codes: 0 completed (or stopped), 1 failed, 2 usage or validation
//! error, 3 paused (or a `stop` the run refuses), 130 SIGINT, 143 SIGTERM.
use crate::args::{CliError, FleetAction, TaskSource, check_prompt, workspace};
use crate::headless::{INTERRUPTED, TERMINATED, denied, invisible, plain, prompt_text, warn};
use octet_core::{ApprovalSummary, Event, event_json};
use octet_fleet::{
    Approver, FleetEvent, RunEnd, RunnerError, RunnerIo, Signals, Start,
    definition::{self, Workflow},
    ownership::OwnershipError,
    record::{AttemptId, CandidateId, Decision, Finding, PauseReason, Record},
    recover, snapshot,
    snapshot::SnapshotError,
    store::{self, StoreError},
    validate::{CoreHost, Resolved, Step, ValidationError, validate},
};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::{
    collections::HashMap,
    future::Future,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use tokio::{
    io::AsyncReadExt,
    signal::unix::{SignalKind, signal},
    sync::{mpsc, watch},
};

/// `println!` that never panics: a closed stdout (a pipe whose reader left)
/// is not worth a panic. See [`stdout`].
macro_rules! outln {
    ($($arg:tt)*) => {
        stdout(&format!("{}\n", format_args!($($arg)*)))
    };
}

/// How many times `run` draws a run ID when it finds the one it drew taken
/// (the ID's random part is 16 bits).
const RUN_ID_TRIES: usize = 3;
/// The longest workflow file read.
const WORKFLOW_LIMIT: u64 = 1024 * 1024;

/// `octet fleet --help`.
pub(crate) fn help() -> &'static str {
    "Octet fleet — multi-step, multi-provider workflows with approval gates\n\
\n\
Usage: octet fleet run WORKFLOW (--task TEXT | --task-file PATH | --task -) [--cwd PATH]\n\
                         [--journal-dir PATH] [--trust] [--output text|json]\n\
       octet fleet resume RUN_ID [--decision D] [--rounds N] [--output text|json]\n\
       octet fleet stop RUN_ID\n\
       octet fleet status [RUN_ID]\n\
       octet fleet validate WORKFLOW [--cwd PATH]\n\
\n\
WORKFLOW is a name (.octet/workflows/NAME.yaml under --cwd) or a path.\n\
run shows what the workflow would execute (each step's binary and mode, every test command)\n\
and asks y/n; the answer is remembered for that exact content. Without a terminal, pass --trust.\n\
--task - reads the task from stdin, so nothing can be asked on the terminal: pass --trust, and\n\
write steps' approvals are denied (use mode: auto or full-access in the profile).\n\
A paused run prints the command that resumes it. On a terminal the decision is asked; otherwise\n\
resume needs --decision: keep, adopt, continue, rounds (with --rounds N), retry, kill, wait,\n\
release or stop. stop leaves the tree as it is and prints the command that discards the changes.\n\
All commands take --journal-dir PATH, where the runs live.\n\
\n\
Exit codes: 0 completed, 1 failed, 2 usage or validation error, 3 paused, 130 SIGINT, 143 SIGTERM.\n"
}

/// Runs one fleet command; the process's exit code. `data_dir` is where runs
/// live (`validate` and `--help` touch no storage and are given none; `run`
/// is given the path, not made yet, and makes it once the work tree is
/// known not to snapshot it), and
/// `explicit` says `--journal-dir` named it, so the commands printed back name
/// it too.
pub(crate) async fn fleet(
    action: FleetAction,
    data_dir: PathBuf,
    explicit: bool,
) -> Result<i32, CliError> {
    let journal_dir = explicit.then(|| data_dir.clone());
    match action {
        FleetAction::Help => {
            stdout(help());
            Ok(0)
        }
        FleetAction::Validate { workflow, cwd } => validate_command(&workflow, cwd).await,
        FleetAction::Status { run } => {
            status_command(&data_dir, run.as_deref(), journal_dir.as_deref()).await
        }
        FleetAction::Stop { run } => {
            let front = Front::new(data_dir, journal_dir, false, false, true)?;
            let decision = Some(Decision::Stop);
            drive(&front, Call::Resume { run, decision }).await
        }
        FleetAction::Resume {
            run,
            decision,
            rounds,
            json,
        } => {
            let decision = decision.map(|name| {
                decision_of(&name, rounds)
                    .ok_or_else(|| CliError::Usage(format!("Unknown decision {name}")))
            });
            let decision = decision.transpose()?;
            let front = Front::new(data_dir, journal_dir, json, terminal(), false)?;
            drive(&front, Call::Resume { run, decision }).await
        }
        FleetAction::Run {
            workflow,
            task,
            cwd,
            trust,
            json,
        } => run_command(data_dir, journal_dir, workflow, task, cwd, trust, json).await,
    }
}

/// What the user decided, as the record spells it. `adopt`'s candidate is
/// assigned by the runner.
fn decision_of(name: &str, rounds: Option<u32>) -> Option<Decision> {
    Some(match name {
        "keep" => Decision::Keep,
        "adopt" => Decision::Adopt {
            candidate: CandidateId(0),
        },
        "continue" => Decision::Continue,
        "rounds" => Decision::Rounds { n: rounds? },
        "retry" => Decision::Retry,
        "kill" => Decision::Kill,
        "wait" => Decision::Wait,
        "release" => Decision::Release { pids: Vec::new() },
        "stop" => Decision::Stop,
        _ => return None,
    })
}

/// Whether the user can be asked: both stdin and stderr are a terminal.
fn terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `text` without control characters, line by line (newlines stay): vendor
/// and workflow text must not steer the terminal.
fn clean(text: &str) -> String {
    text.split('\n').map(plain).collect::<Vec<_>>().join("\n")
}

/// A vendor's streamed reply, as it is safe to show: like [`clean`], but
/// tabs (code's indentation) and the zero-width joiners U+200C and U+200D
/// (Persian, Indic scripts, joined emoji) stay. They only shape what is
/// shown; every other control and invisible character still goes.
fn reply_text(text: &str) -> String {
    text.chars()
        .filter(|&c| {
            matches!(c, '\n' | '\t' | '\u{200C}' | '\u{200D}') || !(c.is_control() || invisible(c))
        })
        .collect()
}

/// `text` as one indented block.
fn block(text: &str, indent: &str) -> String {
    clean(text).replace('\n', &format!("\n{indent}"))
}

/// Writes to stderr; a closed stderr is not worth a failure.
fn say(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

/// `text` as one word for a shell, quoted if it needs it. `None` when it has
/// a control character: that cannot be shown faithfully, and a command that
/// names a different path than the real one must never be printed.
fn shell_word(text: &str) -> Option<String> {
    if text.chars().any(|c| c.is_control() || invisible(c)) {
        return None;
    }
    let plain_word = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:=@%,".contains(c));
    Some(if plain_word {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    })
}

/// `octet fleet resume RUN` as the user would type it to go on from
/// `options`, naming `--journal-dir` when one was given.
fn resume_command(run: &str, options: &[&str], journal_dir: Option<&Path>) -> String {
    let dir = journal_dir.map(|dir| shell_word(&dir.display().to_string()));
    let (Some(run), true) = (shell_word(run), dir.as_ref().is_none_or(Option::is_some)) else {
        return "octet fleet resume RUN_ID (the run ID or the journal directory has a control \
                character, so the exact command is not shown)"
            .into();
    };
    let mut command = format!("octet fleet resume {run}");
    if !options.is_empty() {
        let _ = write!(command, " --decision <{}>", options.join("|"));
    }
    if options.contains(&"rounds") {
        command.push_str(" [--rounds N]");
    }
    if let Some(dir) = dir.flatten() {
        let _ = write!(command, " --journal-dir {dir}");
    }
    command
}

/// What discarding a stopped run's changes does.
const DISCARD: &str = "to discard the run's changes: this restores the work tree to the run's \
starting snapshot (so it also discards any edit made since), removes untracked files that are not \
in that snapshot (ignored files are kept) and leaves the index unstaged";

/// The command that discards a run's changes: the work tree at `top` back to
/// the snapshot `tree`, then the index back to `HEAD`. `None` when a path has
/// a control character and so cannot be shown safely.
fn discard_command(top: &str, tree: &str) -> Option<String> {
    let (top, tree) = (shell_word(top)?, shell_word(tree)?);
    Some(format!(
        "git -C {top} read-tree --reset -u {tree} && git -C {top} clean -fd && git -C {top} reset -q"
    ))
}

/// The serialized name of `value` (`reconcile`, `drift`, …).
fn name_of(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

// ---- workflows ---------------------------------------------------------

/// The file `spec` names: a path (as a shell reads it, relative to the current
/// directory), or a bare name under `<cwd>/.octet/workflows`.
fn workflow_path(spec: &str, cwd: &Path) -> PathBuf {
    let yaml = Path::new(spec)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml") || ext.eq_ignore_ascii_case("yml"));
    let path = spec.contains('/') || yaml;
    if path {
        PathBuf::from(spec)
    } else {
        cwd.join(".octet/workflows").join(format!("{spec}.yaml"))
    }
}

/// Reads and parses the workflow `spec` names.
async fn load_workflow(spec: &str, cwd: &Path) -> Result<Workflow, CliError> {
    let path = workflow_path(spec, cwd);
    let shown = plain(&path.display().to_string());
    let usage = |message: String| CliError::Usage(format!("{shown}: {message}"));
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| usage(format!("cannot read the workflow: {e}")))?;
    let mut bytes = Vec::new();
    file.take(WORKFLOW_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| usage(format!("cannot read the workflow: {e}")))?;
    if bytes.len() as u64 > WORKFLOW_LIMIT {
        return Err(usage("the workflow is over 1 MiB".into()));
    }
    let yaml =
        String::from_utf8(bytes).map_err(|_| usage("the workflow is not UTF-8 text".into()))?;
    definition::parse(&yaml).map_err(|e| usage(plain(&e.to_string())))
}

/// Every error, or the validated workflow; a relative `binary` is found
/// under `cwd`.
fn validated(workflow: &Workflow, cwd: &Path) -> Result<Resolved, Vec<ValidationError>> {
    validate(workflow, &CoreHost::in_dir(cwd))
}

fn report_errors(errors: &[ValidationError]) -> i32 {
    for error in errors {
        warn(&plain(&error.to_string()));
    }
    2
}

/// `validate`: static, side-effect free; every error, not only the first.
async fn validate_command(spec: &str, cwd: Option<PathBuf>) -> Result<i32, CliError> {
    let cwd = workspace(&match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().map_err(|e| CliError::Run(e.to_string()))?,
    })?;
    let workflow = load_workflow(spec, &cwd).await?;
    match validated(&workflow, &cwd) {
        Ok(resolved) => {
            outln!(
                "{}: valid, {} steps",
                plain(&resolved.name),
                resolved.steps.len()
            );
            Ok(0)
        }
        Err(errors) => Ok(report_errors(&errors)),
    }
}

// ---- trust -------------------------------------------------------------

/// What the workflow would execute, for the trust prompt: each step's engine,
/// absolute binary and mode, and every test command.
///
/// Every value is shown whole and as it is: a character that would hide or
/// reorder text (a control character, a bidirectional or zero-width one) is
/// shown escaped (`\u{202e}`), a binary path's line break too, and each line
/// of a multi-line command on a line of its own.
fn trust_summary(resolved: &Resolved) -> String {
    let mut text = format!("Workflow {} would run:\n", visible(&resolved.name));
    for Step {
        id, profile, refs, ..
    } in &resolved.steps
    {
        let _ = writeln!(
            text,
            "  {}: {} {} (mode {})",
            visible(id),
            visible(&profile.engine),
            visible(&profile.binary.display().to_string()),
            visible(&profile.mode)
        );
        if let octet_fleet::validate::Refs::Tests { commands, .. } = refs {
            for command in commands {
                let lines: Vec<String> = command.split('\n').map(visible).collect();
                let _ = writeln!(text, "      $ {}", lines.join("\n        "));
            }
        }
    }
    text
}

/// `text` on one line with every control or invisible formatting character
/// (a line break too) escaped as `\u{…}`, so what is shown is what is there.
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || invisible(c) {
                format!("\\u{{{:x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Where the trust record of workflow `hash` is.
fn trust_path(data_dir: &Path, hash: &str) -> Result<PathBuf, CliError> {
    if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CliError::Run(format!("Unexpected workflow hash {hash:?}")));
    }
    Ok(data_dir.join("fleet").join("trusted").join(hash))
}

/// Records that the workflow with `hash` may run, owner-only.
async fn remember_trust(data_dir: &Path, hash: &str, summary: &str) -> Result<(), CliError> {
    let path = trust_path(data_dir, hash)?;
    let fail = |e: std::io::Error| CliError::Run(format!("Cannot record the trust decision: {e}"));
    if let Some(parent) = path.parent() {
        octet_store::create_private_dir(parent)
            .await
            .map_err(fail)?;
    }
    match octet_store::create_private(&path).await {
        Ok(mut file) => {
            tokio::io::AsyncWriteExt::write_all(&mut file, summary.as_bytes())
                .await
                .map_err(fail)?;
            file.sync_all().await.map_err(fail)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(fail(e)),
    }
}

// ---- reading the user --------------------------------------------------

/// The lines of an input, read by one thread for the life of the process and
/// handed over a channel. Every prompt takes its answer from here, so a
/// prompt that is dropped (an approval that closed, a signal) leaves no reader
/// behind to swallow the next answer; what was typed for it is still queued,
/// and the next prompt discards it ([`LineSource::drain`]) before it asks.
struct LineSource {
    lines: mpsc::UnboundedReceiver<String>,
}

impl LineSource {
    /// Reads lines with `read_line` on a thread of its own, so a prompt nobody
    /// answers cannot hold the process open. The end of input closes the
    /// channel.
    fn new(
        mut read_line: impl FnMut(&mut String) -> std::io::Result<usize> + Send + 'static,
    ) -> Self {
        let (tx, lines) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                match read_line(&mut line) {
                    Ok(bytes) if bytes > 0 && tx.send(line).is_ok() => {}
                    _ => break,
                }
            }
        });
        Self { lines }
    }

    /// Drops every line already waiting: answers to prompts that are gone.
    fn drain(&mut self) {
        while self.lines.try_recv().is_ok() {}
    }

    /// The next line; `None` at the end of input. Cancel-safe.
    async fn next(&mut self) -> Option<String> {
        self.lines.recv().await
    }
}

/// Asks the user questions on stderr and reads the answers from stdin, one
/// at a time. Cloned into the approver; the stdin reader starts with the
/// first question.
#[derive(Clone)]
struct Prompter {
    source: Arc<tokio::sync::Mutex<Option<LineSource>>>,
}

impl Prompter {
    fn new() -> Self {
        Self {
            source: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Shows `prompt` and reads the answer; `None` at end of input. Cancel-safe.
    async fn ask(&self, prompt: &str) -> Option<String> {
        let mut source = self.source.lock().await;
        let source =
            source.get_or_insert_with(|| LineSource::new(|line| std::io::stdin().read_line(line)));
        source.drain();
        say(prompt);
        source.next().await
    }
}

/// Whether `answer` says yes.
fn yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

/// The first signal `signals` holds or gets.
async fn next_signal(signals: &mut Signals) -> &'static str {
    loop {
        if let Some(name) = *signals.borrow_and_update() {
            return name;
        }
        if signals.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Registers SIGINT and SIGTERM, before anything starts so one that comes
/// while a run opens is not lost, and forwards them to the runner's channel.
fn watch_signals() -> Result<Signals, CliError> {
    let fail = |e: std::io::Error| CliError::Run(format!("Cannot watch for signals: {e}"));
    let mut interrupt = signal(SignalKind::interrupt()).map_err(fail)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(fail)?;
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                _ = interrupt.recv() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            };
            if tx.send(Some(name)).is_err() {
                break;
            }
        }
    });
    Ok(rx)
}

/// The exit code of a run ended by `signal`.
fn signal_code(signal: &str) -> i32 {
    if signal == "SIGTERM" {
        TERMINATED
    } else {
        INTERRUPTED
    }
}

// ---- approvals ---------------------------------------------------------

/// Answers write steps' approvals: y/n on stderr on a terminal, otherwise
/// denied with a warning, as `--print` does.
struct Terminal {
    ask: bool,
    prompter: Prompter,
    console: Console,
}

impl Approver for Terminal {
    fn ask<'a>(
        &'a mut self,
        summary: &'a ApprovalSummary,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            if !self.ask {
                self.console.warn(&format!(
                    "denied an approval ({}); nothing can ask. Set mode: auto or full-access \
                     in the profile for unattended runs.",
                    denied(summary)
                ));
                return false;
            }
            let mut text = format!("\napproval: {}\n", plain(&summary.action));
            if let Some(command) = &summary.command {
                let _ = writeln!(text, "  command: {}", block(command, "           "));
            }
            for file in &summary.files {
                let _ = writeln!(text, "  file: {}", plain(file));
            }
            if let Some(cwd) = &summary.cwd {
                let _ = writeln!(text, "  in: {}", plain(cwd));
            }
            let _ = write!(
                text,
                "  allowing grants: {}\nAllow? [y/N] ",
                plain(&summary.scope)
            );
            self.prompter
                .ask(&text)
                .await
                .is_some_and(|answer| yes(&answer))
        })
    }
}

// ---- showing events ----------------------------------------------------

/// Stdout and stderr as a terminal shows them, in one place: whatever the
/// renderer and the approver write goes through here, so a warning on stderr
/// starts on a line of its own even while a reply streams on stdout.
#[derive(Clone)]
struct Console(Arc<std::sync::Mutex<Streams>>);

struct Streams {
    stdout: Box<dyn Write + Send>,
    stderr: Box<dyn Write + Send>,
    /// The text on stdout is not at the start of a line.
    mid_line: bool,
}

impl Console {
    fn new() -> Self {
        Self::with_output(Box::new(std::io::stdout()), Box::new(std::io::stderr()))
    }

    fn with_output(stdout: Box<dyn Write + Send>, stderr: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(Streams {
            stdout,
            stderr,
            mid_line: false,
        })))
    }

    fn streams(&self) -> std::sync::MutexGuard<'_, Streams> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `text` on stdout as it is.
    fn out(&self, text: &str) {
        let mut streams = self.streams();
        let _ = streams.stdout.write_all(text.as_bytes());
        let _ = streams.stdout.flush();
        if !text.is_empty() {
            streams.mid_line = !text.ends_with('\n');
        }
    }

    /// Ends stdout's line, if it is mid-line.
    fn end_line(&self) {
        let mut streams = self.streams();
        if streams.mid_line {
            let _ = streams.stdout.write_all(b"\n");
            let _ = streams.stdout.flush();
            streams.mid_line = false;
        }
    }

    /// A warning on stderr as `--print` writes it, on a line of its own: a
    /// reply rarely ends with a newline.
    fn warn(&self, text: &str) {
        self.end_line();
        let mut streams = self.streams();
        let _ = writeln!(streams.stderr, "octet: {text}");
        let _ = streams.stderr.flush();
    }
}

/// `event` as one JSON object. A child's event goes through
/// [`event_json`]; the caller adds the step to it.
pub(crate) fn fleet_event_json(event: &FleetEvent) -> Value {
    match event {
        FleetEvent::RunStarted { run } => json!({"type": "run_started", "run": run}),
        FleetEvent::AttemptStarted {
            attempt,
            step,
            round,
            engine,
            model,
        } => json!({
            "type": "attempt_started", "attempt": attempt.0, "step": step,
            "round": round, "engine": engine, "model": model,
        }),
        FleetEvent::Session { attempt, event } => {
            json!({"type": "session", "attempt": attempt.0, "event": event_json(event)})
        }
        FleetEvent::Candidate { id, origin } => {
            json!({"type": "candidate", "id": id.0, "origin": name_of(origin)})
        }
        FleetEvent::EvidenceCommand { index, command } => {
            json!({"type": "evidence_command", "index": index, "command": command})
        }
        FleetEvent::AttemptFinished { attempt, outcome } => {
            json!({"type": "attempt_finished", "attempt": attempt.0, "outcome": outcome})
        }
        FleetEvent::Retrying { attempt, at } => {
            json!({"type": "retrying", "attempt": attempt.0, "at": at})
        }
        FleetEvent::Paused { reason, detail } => {
            json!({"type": "paused", "reason": name_of(reason), "detail": detail})
        }
        FleetEvent::NeedsDecision { reason, options } => {
            json!({"type": "needs_decision", "reason": name_of(reason), "options": options})
        }
        FleetEvent::Completed {
            tree,
            diff_command,
            open_findings,
        } => json!({
            "type": "completed", "tree": tree, "diff_command": diff_command,
            "open_findings": open_findings,
        }),
    }
}

/// Shows a call's events as they come, and keeps what the end needs.
struct Renderer {
    json: bool,
    /// The step each attempt runs, to wrap its session's events.
    steps: HashMap<AttemptId, String>,
    /// The run's ID, once the run said it.
    run: Option<String>,
    /// Anything but the start and a pause happened.
    progress: bool,
    /// The command that shows a completed run's change.
    diff_command: Option<String>,
    /// The findings a completed run left open.
    open_findings: Vec<Finding>,
    /// Where events go: stdout, and the vendor's notices and errors to
    /// stderr.
    console: Console,
}

impl Renderer {
    fn new(json: bool, console: Console) -> Self {
        Self {
            json,
            steps: HashMap::new(),
            run: None,
            progress: false,
            diff_command: None,
            open_findings: Vec::new(),
            console,
        }
    }

    /// Shows events until the run's sender is gone.
    async fn consume(&mut self, mut events: mpsc::Receiver<FleetEvent>) {
        while let Some(event) = events.recv().await {
            self.show(&event);
        }
        self.end_line();
    }

    fn out(&self, text: &str) {
        self.console.out(text);
    }

    /// A vendor's notice or error, on stderr on a line of its own.
    fn warn(&self, text: &str) {
        self.console.warn(text);
    }

    fn end_line(&self) {
        self.console.end_line();
    }

    fn line(&self, text: &str) {
        self.end_line();
        self.out(&format!("{text}\n"));
    }

    fn show(&mut self, event: &FleetEvent) {
        match event {
            FleetEvent::RunStarted { run } => self.run = Some(run.clone()),
            FleetEvent::Paused { .. } | FleetEvent::NeedsDecision { .. } => {}
            _ => self.progress = true,
        }
        if let FleetEvent::AttemptStarted { attempt, step, .. } = event {
            self.steps.insert(*attempt, step.clone());
        }
        if let FleetEvent::Completed {
            diff_command,
            open_findings,
            ..
        } = event
        {
            self.diff_command = Some(diff_command.clone());
            self.open_findings.clone_from(open_findings);
        }
        if self.json {
            let mut value = fleet_event_json(event);
            if let FleetEvent::Session { attempt, .. } = event
                && let Some(step) = self.steps.get(attempt)
            {
                value["step"] = json!(step);
            }
            self.line(&value.to_string());
        } else {
            self.show_text(event);
        }
    }

    fn show_text(&self, event: &FleetEvent) {
        match event {
            FleetEvent::RunStarted { run } => self.line(&format!("run {}", plain(run))),
            FleetEvent::AttemptStarted {
                step,
                round,
                engine,
                model,
                ..
            } => {
                let model = model
                    .as_deref()
                    .map(|m| format!(" {}", plain(m)))
                    .unwrap_or_default();
                self.line(&format!(
                    "▸ {} · round {round} · {}{model}",
                    plain(step),
                    plain(engine)
                ));
            }
            FleetEvent::Session { event, .. } => match event {
                Event::Text(text) => self.out(&reply_text(text)),
                Event::Notice(text) | Event::Error(text) => self.warn(&plain(text)),
                _ => {}
            },
            FleetEvent::Candidate { id, origin } => {
                self.line(&format!("  candidate {} ({})", id.0, name_of(origin)));
            }
            FleetEvent::EvidenceCommand { command, .. } => {
                self.line(&format!("  $ {}", block(command, "    ")));
            }
            FleetEvent::AttemptFinished { outcome, .. } => {
                self.line(&format!("  → {}", plain(outcome)));
            }
            FleetEvent::Retrying { at, .. } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                self.line(&format!(
                    "  transient failure; retrying in {}s",
                    at.saturating_sub(now)
                ));
            }
            FleetEvent::Paused { reason, detail } => {
                self.line(&format!(
                    "paused ({}):\n  {}",
                    name_of(reason),
                    block(detail, "  ")
                ));
            }
            FleetEvent::NeedsDecision { .. } | FleetEvent::Completed { .. } => {}
        }
    }
}

// ---- driving a run -----------------------------------------------------

/// What one trip into the runner does.
enum Call {
    Start(Box<Start>),
    Resume {
        run: String,
        decision: Option<Decision>,
    },
}

/// What the commands that drive a run share.
struct Front {
    data_dir: PathBuf,
    /// `--journal-dir`, for the commands printed back.
    journal_dir: Option<PathBuf>,
    json: bool,
    /// The user can be asked.
    interactive: bool,
    /// The call is `stop`, whose refusal is a pause, not a usage error.
    stopping: bool,
    signals: Signals,
    prompter: Prompter,
    console: Console,
}

impl Front {
    fn new(
        data_dir: PathBuf,
        journal_dir: Option<PathBuf>,
        json: bool,
        interactive: bool,
        stopping: bool,
    ) -> Result<Self, CliError> {
        Ok(Self {
            journal_dir,
            data_dir,
            json,
            interactive,
            stopping,
            signals: watch_signals()?,
            prompter: Prompter::new(),
            console: Console::new(),
        })
    }

    fn io(&self, events: mpsc::Sender<FleetEvent>) -> RunnerIo {
        let approver = Terminal {
            ask: self.interactive,
            prompter: self.prompter.clone(),
            console: self.console.clone(),
        };
        RunnerIo::new(events, Box::new(approver), self.signals.clone())
    }

    /// Makes `call`; its events go to `events`, which is dropped at the end.
    async fn call(
        &self,
        call: &Call,
        events: mpsc::Sender<FleetEvent>,
    ) -> Result<RunEnd, RunnerError> {
        match call {
            Call::Start(start) => {
                let mut result = Err(RunnerError::Refused("no attempt was made".into()));
                for _ in 0..RUN_ID_TRIES {
                    result = octet_fleet::start((**start).clone(), self.io(events.clone())).await;
                    if !matches!(result, Err(RunnerError::Store(StoreError::RunExists(_)))) {
                        break;
                    }
                }
                result
            }
            Call::Resume { run, decision } => {
                octet_fleet::resume(&self.data_dir, run, decision.clone(), self.io(events)).await
            }
        }
    }

    /// The command that resumes `run` from a pause with `options`.
    fn resume_command(&self, run: &str, options: &[&str]) -> String {
        resume_command(run, options, self.journal_dir.as_deref())
    }
}

/// How a call that failed ends the command: an error, or the code of a
/// refused `stop`.
fn failure(error: RunnerError, stopping: bool) -> Result<i32, CliError> {
    match error {
        RunnerError::Refused(reason) if stopping => {
            warn(&plain(&reason));
            Ok(3)
        }
        other => Err(cli_error(other)),
    }
}

/// `error` as the command line's: what the user asked for is a usage error,
/// whatever else went wrong a failure. A run refused because another Octet
/// drives it or another run owns the work tree ran nothing, so that is a
/// usage error too; those refusals come only before a run starts or resumes.
fn cli_error(error: RunnerError) -> CliError {
    match error {
        RunnerError::Refused(reason) => CliError::Usage(plain(&reason)),
        RunnerError::Ownership(
            error @ (OwnershipError::RunBusy
            | OwnershipError::WorktreeBusy { .. }
            | OwnershipError::OtherRunUnresolved { .. }),
        ) => CliError::Usage(plain(&error.to_string())),
        RunnerError::Store(StoreError::NoSuchRun(run)) => {
            CliError::Usage(format!("No such run: {}", plain(&run)))
        }
        RunnerError::Snapshot(
            error @ (SnapshotError::NotARepository(_)
            | SnapshotError::SparseCheckout(_)
            | SnapshotError::DataDirInTree { .. }),
        ) => CliError::Usage(plain(&error.to_string())),
        other => CliError::Run(plain(&other.to_string())),
    }
}

/// Makes `first` and, while the user picks decisions at pauses, the resumes
/// that follow; the exit code.
async fn drive(front: &Front, first: Call) -> Result<i32, CliError> {
    let mut call = first;
    loop {
        let (events, received) = mpsc::channel(64);
        let mut renderer = Renderer::new(front.json, front.console.clone());
        let (result, ()) = tokio::join!(front.call(&call, events), renderer.consume(received));
        let given = matches!(
            &call,
            Call::Resume {
                decision: Some(_),
                ..
            }
        );
        let resumed = matches!(&call, Call::Resume { .. });
        let run = match &call {
            Call::Resume { run, .. } => Some(run.clone()),
            Call::Start(_) => renderer.run.clone(),
        };
        let end = match result {
            Ok(end) => end,
            Err(error) => return failure(error, front.stopping),
        };
        let run = run.unwrap_or_default();
        match end {
            RunEnd::Completed => {
                if !front.json {
                    completed(
                        front,
                        &run,
                        renderer.diff_command.as_deref(),
                        &renderer.open_findings,
                    )
                    .await;
                }
                return Ok(0);
            }
            RunEnd::Failed(error) => {
                warn(&format!("run {} failed: {}", plain(&run), plain(&error)));
                return Ok(1);
            }
            RunEnd::Stopped => {
                stopped(front, &run).await;
                return Ok(0);
            }
            RunEnd::Interrupted(name) => {
                hint(
                    front,
                    &format!(
                        "run {} is paused; resume it with: {}",
                        plain(&run),
                        front.resume_command(&run, &[])
                    ),
                );
                return Ok(signal_code(name));
            }
            RunEnd::Paused(reason) => {
                let options = recover::options(reason);
                let decision = if front.interactive && !options.is_empty() {
                    match choose(front, reason, &options).await {
                        Choice::Decision(decision) => Some(decision),
                        Choice::Signal(name) => return Ok(signal_code(name)),
                        Choice::None => None,
                    }
                } else {
                    None
                };
                if let Some(decision) = decision {
                    call = Call::Resume {
                        run,
                        decision: Some(decision),
                    };
                    continue;
                }
                if resumed && !given && !renderer.progress && !options.is_empty() {
                    return Err(CliError::Usage(format!(
                        "Run {} is paused ({}); --decision is required: {}",
                        plain(&run),
                        name_of(&reason),
                        options.join(", ")
                    )));
                }
                hint(
                    front,
                    &format!("resume it with: {}", front.resume_command(&run, &options)),
                );
                return Ok(3);
            }
        }
    }
}

/// A line of guidance: on stdout in text mode, on stderr with JSON lines
/// there.
fn hint(front: &Front, text: &str) {
    if front.json {
        warn(text);
    } else {
        outln!("{text}");
    }
}

/// What the user chose at a pause.
enum Choice {
    Decision(Decision),
    /// A signal came while asking.
    Signal(&'static str),
    /// Input ended: leave the run paused.
    None,
}

/// Offers `options` on stderr and reads the choice, until it is one.
async fn choose(front: &Front, reason: PauseReason, options: &[&'static str]) -> Choice {
    let mut signals = front.signals.clone();
    let mut text = format!("\nThe run is paused ({}). Decisions:\n", name_of(&reason));
    for (number, option) in options.iter().enumerate() {
        let _ = writeln!(text, "  {}) {option}", number + 1);
    }
    text.push_str("Decision (number or name): ");
    loop {
        let line = tokio::select! {
            line = front.prompter.ask(&text) => line,
            name = next_signal(&mut signals) => return Choice::Signal(name),
        };
        let Some(line) = line else {
            return Choice::None;
        };
        let answer = line.trim();
        let picked = answer
            .parse::<usize>()
            .ok()
            .and_then(|number| options.get(number.checked_sub(1)?))
            .or_else(|| options.iter().find(|option| **option == answer));
        let Some(&picked) = picked else {
            say(&format!("Not one of: {}\n", options.join(", ")));
            text = "Decision (number or name): ".into();
            continue;
        };
        let rounds = if picked == "rounds" {
            let Some(answer) = front
                .prompter
                .ask("Raise the cap to how many rounds? ")
                .await
            else {
                return Choice::None;
            };
            let Some(rounds) = answer.trim().parse::<u32>().ok().filter(|n| *n >= 1) else {
                say("Rounds must be a whole number of at least 1\n");
                text = "Decision (number or name): ".into();
                continue;
            };
            Some(rounds)
        } else {
            None
        };
        if let Some(decision) = decision_of(picked, rounds) {
            return Choice::Decision(decision);
        }
    }
}

/// The end of a completed run: the invariant's parts, as the log recorded
/// them, the command that shows the change, and the findings left open.
async fn completed(front: &Front, run: &str, diff_command: Option<&str>, open: &[Finding]) {
    let read = async {
        let (dir, file) = store::open(&front.data_dir, run).await.ok()?;
        let records = store::read_log(&dir).await.ok()?;
        Some((file, records))
    };
    let Some((file, records)) = read.await else {
        warn(&format!(
            "run {} completed, but its log cannot be read",
            plain(run)
        ));
        let mut text = String::new();
        open_findings_text(&mut text, open);
        stdout(&text);
        return;
    };
    stdout(&completed_text(
        run,
        &file.base_tree,
        &records,
        diff_command,
        open,
    ));
}

/// [`completed`]'s text.
fn completed_text(
    run: &str,
    base_tree: &str,
    records: &[Record],
    diff_command: Option<&str>,
    open: &[Finding],
) -> String {
    let mut text = format!("completed {}\n", plain(run));
    for record in records {
        if let Record::Completed {
            candidate,
            tree,
            evidence,
            approval,
            observed_tree,
        } = record
        {
            let _ = write!(
                text,
                "  candidate {}\n  tree {tree}\n  evidence artifact {}\n  approval artifact {}\n  observed tree {observed_tree}\n",
                candidate.0, evidence.0, approval.0
            );
            let diff =
                diff_command.map_or_else(|| format!("git diff {base_tree} {tree}"), str::to_owned);
            let _ = writeln!(text, "  {}", plain(&diff));
        }
    }
    open_findings_text(&mut text, open);
    text
}

/// The findings a completed run left open (minor ones), one per line, after
/// a heading; nothing when there are none.
fn open_findings_text(text: &mut String, open: &[Finding]) {
    if open.is_empty() {
        return;
    }
    let _ = writeln!(text, "open findings ({}):", open.len());
    for finding in open {
        let file = finding
            .file
            .as_deref()
            .map(|file| format!(" ({})", plain(file)))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "  {} [{}] {}{file}",
            plain(&finding.id),
            name_of(&finding.severity),
            block(&finding.summary, "    ")
        );
    }
}

/// Writes `text` to stdout; a closed stdout (a pipe whose reader left) is
/// not an error worth a panic.
fn stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// The end of a stopped run: the tree is as the run left it, and this
/// command would discard its changes.
async fn stopped(front: &Front, run: &str) {
    outln!(
        "stopped {}; the work tree is as the run left it",
        plain(run)
    );
    let Ok((_, file)) = store::open(&front.data_dir, run).await else {
        return;
    };
    match discard_command(&file.top.display().to_string(), &file.base_tree) {
        Some(command) => outln!("{DISCARD}:\n  {command}"),
        None => outln!(
            "the work tree's path has a control character, so no command is shown; the run's \
             starting snapshot is the tree {} (ref refs/octet-fleet/{}/0)",
            plain(&file.base_tree),
            plain(run)
        ),
    }
}

// ---- run, status -------------------------------------------------------

async fn run_command(
    data_dir: PathBuf,
    journal_dir: Option<PathBuf>,
    spec: String,
    task: TaskSource,
    cwd: Option<PathBuf>,
    trust: bool,
    json: bool,
) -> Result<i32, CliError> {
    let cwd = workspace(&match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().map_err(|e| CliError::Run(e.to_string()))?,
    })?;
    let from_stdin = task == TaskSource::Stdin;
    let task = match task {
        TaskSource::Text(text) => text,
        TaskSource::Stdin => {
            let task = crate::headless::read_prompt().await?;
            check_prompt(&task)?;
            task
        }
        TaskSource::File(path) => {
            let limit = octet_core::PROMPT_LIMIT as u64 + 3;
            let mut bytes = Vec::new();
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| CliError::Usage(format!("Cannot read the task file: {e}")))?;
            file.take(limit)
                .read_to_end(&mut bytes)
                .await
                .map_err(|e| CliError::Usage(format!("Cannot read the task file: {e}")))?;
            let task = prompt_text(bytes)?;
            check_prompt(&task)?;
            task
        }
    };
    let workflow = load_workflow(&spec, &cwd).await?;
    let resolved = match validated(&workflow, &cwd) {
        Ok(resolved) => resolved,
        Err(errors) => return Ok(report_errors(&errors)),
    };
    let snapshot_error = |e: SnapshotError| match e {
        SnapshotError::NotARepository(_)
        | SnapshotError::SparseCheckout(_)
        | SnapshotError::DataDirInTree { .. } => CliError::Usage(plain(&e.to_string())),
        other => CliError::Run(plain(&other.to_string())),
    };
    let repo = snapshot::open_repo(&cwd).await.map_err(snapshot_error)?;
    // Before the data directory is made (`data_dir` is only its path so
    // far) and the trust record written there.
    snapshot::check_data_dir(&repo, &data_dir)
        .await
        .map_err(snapshot_error)?;
    let data_dir = crate::make_data_dir(&data_dir)?;
    let journal_dir = journal_dir.map(|_| data_dir.clone());
    let bytes = serde_json::to_vec(&resolved).map_err(|e| CliError::Run(e.to_string()))?;
    let hash = store::workflow_hash(&repo.top, &bytes)
        .await
        .map_err(|e| CliError::Run(plain(&e.to_string())))?;
    // The terminal answers questions unless stdin carried the task.
    let interactive = terminal() && !from_stdin;
    let signals = watch_signals()?;
    let prompter = Prompter::new();
    let known = tokio::fs::try_exists(trust_path(&data_dir, &hash)?)
        .await
        .unwrap_or(false);
    if !known {
        let summary = trust_summary(&resolved);
        say(&format!("{summary}\n"));
        let allowed = if trust {
            true
        } else if interactive {
            let mut signals = signals.clone();
            let answer = tokio::select! {
                answer = prompter.ask("Trust this workflow and run it? [y/N] ") => answer,
                name = next_signal(&mut signals) => return Ok(signal_code(name)),
            };
            answer.is_some_and(|answer| yes(&answer))
        } else {
            return Err(CliError::Usage(
                "Running a workflow is a trust decision, and nothing here can ask. \
                 Review the above and pass --trust."
                    .into(),
            ));
        };
        if !allowed {
            say("Not trusted; nothing was started.\n");
            return Ok(2);
        }
        remember_trust(&data_dir, &hash, &summary).await?;
    }
    let start = Start {
        workflow: resolved,
        hash,
        task,
        cwd,
        data_dir: data_dir.clone(),
    };
    let front = Front {
        data_dir,
        journal_dir,
        json,
        interactive,
        stopping: false,
        signals,
        prompter,
        console: Console::new(),
    };
    drive(&front, Call::Start(Box::new(start))).await
}

/// `status`: every run in a table, or one run in detail.
async fn status_command(
    data_dir: &Path,
    run: Option<&str>,
    journal_dir: Option<&Path>,
) -> Result<i32, CliError> {
    let runs = octet_fleet::status(data_dir, run)
        .await
        .map_err(cli_error)?;
    if let (Some(_), [summary]) = (run, runs.as_slice()) {
        outln!("run:       {}", plain(&summary.id));
        outln!("workflow:  {}", plain(&summary.workflow));
        outln!("state:     {}", summary.state);
        match &summary.step {
            Some(step) => outln!("step:      {} (round {})", plain(step), summary.round),
            None => outln!("step:      -"),
        }
        outln!("findings:  {} open", summary.open_findings);
        for reference in &summary.refs {
            outln!("ref:       {}", plain(reference));
        }
        if summary.state == "paused" {
            outln!(
                "resume:    {}",
                resume_command(&summary.id, &[], journal_dir)
            );
        }
        return Ok(0);
    }
    if runs.is_empty() {
        outln!("No runs");
        return Ok(0);
    }
    let rows: Vec<[String; 6]> = runs
        .iter()
        .map(|s| {
            [
                plain(&s.id),
                plain(&s.workflow),
                s.state.clone(),
                s.step.as_deref().map_or_else(|| "-".into(), plain),
                s.round.to_string(),
                s.open_findings.to_string(),
            ]
        })
        .collect();
    let header = ["RUN", "WORKFLOW", "STATE", "STEP", "ROUND", "OPEN"].map(str::to_owned);
    let widths: Vec<usize> = (0..6)
        .map(|column| {
            std::iter::once(&header)
                .chain(&rows)
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in std::iter::once(&header).chain(&rows) {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        outln!("{}", cells.join("  ").trim_end());
    }
    for summary in &runs {
        if let Some(error) = &summary.error {
            warn(&format!(
                "run {} cannot be read: {}",
                plain(&summary.id),
                plain(error)
            ));
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::DECISIONS;

    #[test]
    fn a_streamed_reply_keeps_tabs_and_joiners() {
        // A Makefile recipe, a Persian word with a ZWNJ, a joined emoji.
        let text = "all:\n\tcargo test\nمی\u{200C}خواهم 👩\u{200D}💻";
        assert_eq!(reply_text(text), text);
        // Still dropped: other controls, bidi controls and the rest of the
        // invisible set.
        let hostile = "a\u{1b}[2J\rb\u{202E}c\u{2066}d\u{200B}e\u{E0041}f\u{7}g";
        assert_eq!(reply_text(hostile), "a[2Jbcdefg");
    }

    #[test]
    fn every_decision_name_maps_to_a_decision() {
        for name in DECISIONS {
            let rounds = (name == "rounds").then_some(2);
            assert!(decision_of(name, rounds).is_some(), "{name}");
        }
        assert_eq!(decision_of("rounds", None), None);
        assert_eq!(
            decision_of("rounds", Some(4)),
            Some(Decision::Rounds { n: 4 })
        );
        assert_eq!(decision_of("bogus", None), None);
    }

    #[test]
    fn vendor_text_loses_its_control_characters_but_keeps_its_lines() {
        assert_eq!(
            clean("a\u{1b}]0;x\u{7}b\nsecond\r\nthird\u{9b}"),
            "a]0;xb\nsecond\nthird"
        );
        assert_eq!(block("one\ntwo", "  "), "one\n  two");
    }

    #[test]
    fn the_trust_prompt_shows_every_value_whole_and_as_it_is() {
        struct Anywhere;
        impl octet_fleet::validate::Host for Anywhere {
            fn engine_known(&self, _: &str) -> bool {
                true
            }
            fn check_effort(&self, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn resolve_binary(&self, engine: &str, _: Option<&Path>) -> Option<PathBuf> {
                Some(PathBuf::from(format!("/bin/{engine}")))
            }
        }
        assert_eq!(visible("/bin/cl\u{200b}aude"), "/bin/cl\\u{200b}aude");
        assert_eq!(visible("rm \u{202e}gpj.sh"), "rm \\u{202e}gpj.sh");
        assert_eq!(visible("/bin/a\n/bin/b"), "/bin/a\\u{a}/bin/b");
        assert_eq!(visible("naïve — ok"), "naïve — ok");
        let shipped = include_str!("../../../docs/fleet/research-plan-build.yaml");
        let workflow = definition::parse(shipped).unwrap();
        let mut resolved = validate(&workflow, &Anywhere).unwrap();
        resolved.steps[0].profile.binary = PathBuf::from("/opt/cl\u{202e}de\nsh");
        for step in &mut resolved.steps {
            if let octet_fleet::validate::Refs::Tests { commands, .. } = &mut step.refs {
                *commands = vec!["make test\nrm -rf \u{200b}/tmp/x".into()];
            }
        }
        let text = trust_summary(&resolved);
        assert!(text.contains("/opt/cl\\u{202e}de\\u{a}sh"), "{text}");
        assert!(
            text.contains("      $ make test\n        rm -rf \\u{200b}/tmp/x\n"),
            "{text}"
        );
        assert!(
            !text.contains('\u{202e}') && !text.contains('\u{200b}'),
            "{text}"
        );
        assert_eq!(shell_word("/work/\u{202e}x"), None);
    }

    #[test]
    fn shell_words_are_quoted_only_when_they_need_it() {
        assert_eq!(shell_word("/work/repo").as_deref(), Some("/work/repo"));
        assert_eq!(
            shell_word("/my work/it's").as_deref(),
            Some("'/my work/it'\\''s'")
        );
    }

    #[test]
    fn a_path_with_a_control_character_never_becomes_a_command() {
        assert_eq!(shell_word("/work/a\nb"), None);
        assert_eq!(shell_word("/work/a\u{1b}[2Jb"), None);
        assert_eq!(discard_command("/work/repo\n/..", "abc123"), None);
        assert_eq!(discard_command("/work/repo", "abc\u{7}"), None);
        let command = discard_command("/my work", "4b825dc6").unwrap();
        assert_eq!(
            command,
            "git -C '/my work' read-tree --reset -u 4b825dc6 \
             && git -C '/my work' clean -fd && git -C '/my work' reset -q"
        );
        let plain = resume_command("f1-0001", &["continue", "rounds", "stop"], None);
        assert_eq!(
            plain,
            "octet fleet resume f1-0001 --decision <continue|rounds|stop> [--rounds N]"
        );
        let named = resume_command("f1-0001", &[], Some(Path::new("/my data")));
        assert_eq!(named, "octet fleet resume f1-0001 --journal-dir '/my data'");
        let unsafe_dir = resume_command("f1-0001", &[], Some(Path::new("/d\nx")));
        assert!(!unsafe_dir.contains("--journal-dir"), "{unsafe_dir}");
        assert!(resume_command("a\nb", &[], None).contains("not shown"));
    }

    /// A prompter over a socket we type into.
    fn typed() -> (Prompter, std::os::unix::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut input = std::io::BufReader::new(theirs);
        let source = LineSource::new(move |line| std::io::BufRead::read_line(&mut input, line));
        let prompter = Prompter {
            source: Arc::new(tokio::sync::Mutex::new(Some(source))),
        };
        (prompter, ours)
    }

    const PATIENCE: std::time::Duration = std::time::Duration::from_millis(150);

    #[tokio::test]
    async fn an_abandoned_prompt_does_not_swallow_the_next_answer() {
        let (prompter, mut typing) = typed();
        // The first prompt is dropped unanswered, as a closed approval is.
        let abandoned = tokio::time::timeout(PATIENCE, prompter.ask("first? ")).await;
        assert!(abandoned.is_err());
        // What the user typed for it afterwards is stale; the next prompt
        // must not take it, and must take the answer typed for itself.
        typing.write_all(b"stale\n").unwrap();
        tokio::time::sleep(PATIENCE).await;
        let asked = prompter.ask("second? ");
        let answer = async {
            tokio::time::sleep(PATIENCE).await;
            typing.write_all(b"fresh\n").unwrap();
        };
        let (line, ()) = tokio::join!(asked, answer);
        assert_eq!(line.as_deref(), Some("fresh\n"));
    }

    #[tokio::test]
    async fn the_end_of_input_ends_every_prompt() {
        let (prompter, typing) = typed();
        drop(typing);
        assert_eq!(prompter.ask("one? ").await, None);
        assert_eq!(prompter.ask("two? ").await, None);
    }

    #[test]
    fn open_findings_are_reported_at_completion_as_text_and_json() {
        use octet_fleet::record::Severity;
        let open = vec![
            Finding {
                id: "review_change/F2".into(),
                severity: Severity::Minor,
                summary: "Name the constant.\nIt is used twice.".into(),
                file: Some("src/lib.rs".into()),
            },
            Finding {
                id: "review_plan/F1".into(),
                severity: Severity::Minor,
                summary: "A typo\u{1b}[2J.".into(),
                file: None,
            },
        ];
        let records = [Record::Completed {
            candidate: CandidateId(3),
            tree: "t3".into(),
            evidence: octet_fleet::record::ArtifactId(5),
            approval: octet_fleet::record::ArtifactId(6),
            observed_tree: "t3".into(),
        }];
        let text = completed_text("f1-0001", "t0", &records, None, &open);
        assert!(text.contains("  git diff t0 t3\n"), "{text}");
        assert!(
            text.ends_with(
                "open findings (2):\n  review_change/F2 [minor] Name the constant.\n    \
                 It is used twice. (src/lib.rs)\n  review_plan/F1 [minor] A typo[2J.\n"
            ),
            "{text}"
        );
        assert!(!completed_text("f1-0001", "t0", &records, None, &[]).contains("open findings"));
        let event = FleetEvent::Completed {
            tree: "t3".into(),
            diff_command: "git diff t0 t3".into(),
            open_findings: open,
        };
        let value = fleet_event_json(&event);
        assert_eq!(value["type"], "completed");
        assert_eq!(value["open_findings"][0]["id"], "review_change/F2");
        assert_eq!(value["open_findings"][0]["severity"], "minor");
        assert_eq!(value["open_findings"][0]["file"], "src/lib.rs");
        assert_eq!(value["open_findings"][1]["summary"], "A typo\u{1b}[2J.");
        assert_eq!(value["open_findings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_workflow_is_a_path_or_a_name_under_octet_workflows() {
        let cwd = Path::new("/w");
        assert_eq!(
            workflow_path("flow", cwd),
            Path::new("/w/.octet/workflows/flow.yaml")
        );
        assert_eq!(
            workflow_path("plans/flow.yaml", cwd),
            Path::new("plans/flow.yaml")
        );
        assert_eq!(workflow_path("flow.yml", cwd), Path::new("flow.yml"));
        assert_eq!(
            workflow_path("/abs/flow.yaml", cwd),
            Path::new("/abs/flow.yaml")
        );
    }

    /// One buffer behind both of a renderer's outputs, as a terminal shows
    /// stdout and stderr interleaved.
    #[derive(Clone, Default)]
    struct Screen(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Screen {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_vendor_error_starts_on_its_own_line() {
        let screen = Screen::default();
        let mut renderer = Renderer::new(
            false,
            Console::with_output(Box::new(screen.clone()), Box::new(screen.clone())),
        );
        let session = |event| FleetEvent::Session {
            attempt: AttemptId(0),
            event,
        };
        renderer.show(&session(Event::Text("It may not exist.".into())));
        renderer.show(&session(Event::Error("It may not exist.".into())));
        renderer.show(&session(Event::Text("Next".into())));
        renderer.show(&session(Event::Notice("a notice".into())));
        let shown = String::from_utf8(screen.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            shown,
            "It may not exist.\noctet: It may not exist.\nNext\noctet: a notice\n"
        );
    }

    #[tokio::test]
    async fn an_approval_denied_off_a_terminal_starts_on_its_own_line() {
        let screen = Screen::default();
        let console = Console::with_output(Box::new(screen.clone()), Box::new(screen.clone()));
        let mut renderer = Renderer::new(false, console.clone());
        renderer.show(&FleetEvent::Session {
            attempt: AttemptId(0),
            event: Event::Text("I will run the tests".into()),
        });
        let mut approver = Terminal {
            ask: false,
            prompter: Prompter::new(),
            console,
        };
        let mut summary = ApprovalSummary::new("Run a shell command");
        summary.command = Some("make test".into());
        assert!(!approver.ask(&summary).await);
        renderer.show(&FleetEvent::Session {
            attempt: AttemptId(0),
            event: Event::Text("Done".into()),
        });
        let shown = String::from_utf8(screen.0.lock().unwrap().clone()).unwrap();
        assert!(
            shown.starts_with(
                "I will run the tests\noctet: denied an approval (Run a shell command: make test)"
            ),
            "{shown:?}"
        );
        assert!(shown.ends_with("unattended runs.\nDone"), "{shown:?}");
    }

    #[test]
    fn events_are_json_lines_with_their_step() {
        let started = fleet_event_json(&FleetEvent::AttemptStarted {
            attempt: AttemptId(3),
            step: "implement".into(),
            round: 2,
            engine: "codex".into(),
            model: None,
        });
        assert_eq!(started["type"], "attempt_started");
        assert_eq!(started["round"], 2);
        assert_eq!(started["model"], Value::Null);
        let mut renderer = Renderer::new(true, Console::new());
        renderer.show(&FleetEvent::AttemptStarted {
            attempt: AttemptId(3),
            step: "implement".into(),
            round: 2,
            engine: "codex".into(),
            model: None,
        });
        assert_eq!(
            renderer.steps.get(&AttemptId(3)).map(String::as_str),
            Some("implement")
        );
        let paused = fleet_event_json(&FleetEvent::NeedsDecision {
            reason: PauseReason::Reconcile,
            options: vec!["keep", "stop"],
        });
        assert_eq!(paused["reason"], "reconcile");
        assert_eq!(paused["options"], json!(["keep", "stop"]));
    }
}
