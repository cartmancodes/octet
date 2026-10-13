//! Helpers shared by the interface's tests.
use crate::app::App;

/// An interface for `engine`, in /tmp, before any connection.
pub(crate) fn app_for(engine: octet_core::Engine) -> App {
    let config = octet_core::Config::new(engine, engine.as_str(), "/tmp");
    App::new(&config, "journal".into())
}

/// A real `!` command that runs until dropped (which kills it), standing in
/// for one the user started.
pub(crate) fn running_shell(attach: bool) -> crate::shell::Running {
    crate::shell::Running::spawn("sleep 30".into(), ".".into(), attach)
}

/// Brings the session to idle the way the vendor does: a running turn
/// finishes, a connection opens. Panics if no event gets it there.
pub(crate) fn idle(app: &mut App) {
    use crate::app::ConnPhase;
    match app.conn.phase {
        ConnPhase::Running { .. } => app.event(octet_core::Event::Finished {
            outcome: octet_core::Outcome::Completed,
        }),
        ConnPhase::Connecting => app.event(octet_core::Event::Ready {
            session: app.conn.session.clone(),
        }),
        _ => {}
    }
    assert_eq!(
        app.conn.phase,
        ConnPhase::Idle,
        "no event makes this session idle"
    );
}

/// A tool call event, as an adapter sends it.
pub(crate) fn tool(
    id: &str,
    name: &str,
    target: &str,
    phase: octet_core::ToolPhase,
) -> octet_core::Event {
    octet_core::Event::Tool(octet_core::ToolCall {
        id: id.into(),
        name: name.into(),
        target: target.into(),
        phase,
        detail: format!("{name} {target}"),
    })
}

/// An approval event with a shell-command summary.
pub(crate) fn approval(id: u64, detail: &str) -> octet_core::Event {
    octet_core::Event::Approval {
        id,
        detail: detail.into(),
        summary: octet_core::ApprovalSummary {
            command: Some("cargo test".into()),
            cwd: Some("/tmp".into()),
            ..octet_core::ApprovalSummary::new("Run a shell command")
        },
    }
}

/// Ages every waiting approval past the arm delay, as if each had been open that long, so a remote
/// answer is taken at once.
pub(crate) fn arm_remote_answers(app: &mut App) {
    for asked in &mut app.overlay.approvals {
        asked.arrived = tokio::time::Instant::now() - crate::app::APPROVAL_ARM;
    }
}

/// A waiting approval for `id` with a plain summary.
pub(crate) fn asked(id: u64, detail: &str) -> crate::app::Asked {
    crate::app::Asked {
        id,
        detail: detail.into(),
        summary: octet_core::ApprovalSummary::new("Run a shell command"),
        arrived: tokio::time::Instant::now(),
    }
}

/// A stand-in hub on `<run>/hub.sock` answering one request with `reply`; resolves to the request line.
pub(crate) fn serve_hub(
    run: &std::path::Path,
    reply: &'static str,
) -> tokio::task::JoinHandle<String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    std::fs::create_dir_all(run).unwrap();
    let listener = tokio::net::UnixListener::bind(run.join("hub.sock")).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut request = String::new();
        BufReader::new(read).read_line(&mut request).await.unwrap();
        write
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        request
    })
}
