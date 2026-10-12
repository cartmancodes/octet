//! The tailnet owner and the hub's tailnet URL, read from `tailscale status --json`.
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, time::timeout_at};

/// How long the Tailscale CLI gets to answer.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Where the Tailscale CLI lives: on `PATH`, then inside the App Store app.
const PROGRAMS: [&str; 2] = [
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

/// The login of the tailnet user this Mac belongs to, or `override_login` if given.
///
/// # Errors
///
/// Fails with advice when Tailscale does not answer.
pub async fn owner_login(override_login: Option<String>) -> Result<String, String> {
    if let Some(login) = override_login.filter(|login| !login.is_empty()) {
        return Ok(login);
    }
    status()
        .await
        .and_then(|json| parse_owner(&json))
        .ok_or_else(|| "Tailscale isn't running; start it or pass --owner".to_owned())
}

/// The hub's `https://<name>.ts.net` URL while Tailscale is running.
pub async fn tailnet_url() -> Option<String> {
    parse_tailnet_url(&status().await?)
}

/// `tailscale status --json` from the first Tailscale CLI that answers.
async fn status() -> Option<String> {
    let deadline = Instant::now() + TIMEOUT;
    for program in PROGRAMS {
        if let Some(json) = run_until(deadline, program, &["status", "--json"]).await {
            return Some(json);
        }
    }
    None
}

/// Runs one read-only command: stdin and stderr closed, killed at the
/// deadline. None when the program is missing, fails or overruns.
async fn run_until(deadline: Instant, program: &str, args: &[&str]) -> Option<String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let output = timeout_at(deadline.into(), child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The login of the user who owns this node: `User[Self.UserID].LoginName`.
fn parse_owner(status_json: &str) -> Option<String> {
    let status: serde_json::Value = serde_json::from_str(status_json).ok()?;
    let id = match &status["Self"]["UserID"] {
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::String(text) => text.clone(),
        _ => return None,
    };
    let login = status["User"][id.as_str()]["LoginName"].as_str()?;
    (!login.is_empty()).then(|| login.to_owned())
}

/// `https://` and this node's DNS name, without its trailing dot, while the
/// backend is running.
fn parse_tailnet_url(status_json: &str) -> Option<String> {
    let status: serde_json::Value = serde_json::from_str(status_json).ok()?;
    if status["BackendState"] != "Running" {
        return None;
    }
    let name = status["Self"]["DNSName"].as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| format!("https://{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = r#"{"BackendState":"Running","Self":{"UserID":123,"DNSName":"mac.tail1234.ts.net."},
        "User":{"123":{"LoginName":"me@example.com","DisplayName":"Me"},"456":{"LoginName":"other@example.com"}}}"#;

    #[test]
    fn owner_login_reads_tailscale_status() {
        assert_eq!(parse_owner(RUNNING).as_deref(), Some("me@example.com"));
        let stopped = r#"{"BackendState":"Stopped","Self":{"UserID":456},
            "User":{"456":{"LoginName":"other@example.com"}}}"#;
        assert_eq!(parse_owner(stopped).as_deref(), Some("other@example.com"));
        assert_eq!(
            parse_owner(r#"{"BackendState":"Running","Self":{"DNSName":"a."}}"#),
            None
        );
        assert_eq!(
            parse_owner(r#"{"Self":{"UserID":9},"User":{"1":{"LoginName":"x"}}}"#),
            None
        );
        assert_eq!(
            parse_owner(r#"{"Self":{"UserID":1},"User":{"1":{"LoginName":""}}}"#),
            None
        );
        assert_eq!(parse_owner("not json"), None);
    }

    #[test]
    fn the_tailnet_url_needs_a_running_backend() {
        assert_eq!(
            parse_tailnet_url(RUNNING).as_deref(),
            Some("https://mac.tail1234.ts.net")
        );
        let stopped = RUNNING.replace("Running", "Stopped");
        assert_eq!(parse_tailnet_url(&stopped), None);
        assert_eq!(
            parse_tailnet_url(r#"{"BackendState":"Running","Self":{}}"#),
            None
        );
        assert_eq!(
            parse_tailnet_url(r#"{"BackendState":"Running","Self":{"DNSName":"."}}"#),
            None
        );
        assert_eq!(parse_tailnet_url("not json"), None);
    }

    #[tokio::test]
    async fn an_override_names_the_owner_without_asking_tailscale() {
        assert_eq!(
            owner_login(Some("me@example.com".into())).await.as_deref(),
            Ok("me@example.com")
        );
    }

    #[tokio::test]
    async fn a_command_that_overruns_or_fails_gives_nothing() {
        let soon = || Instant::now() + Duration::from_millis(300);
        assert_eq!(
            run_until(soon(), "echo", &["hi"]).await.as_deref(),
            Some("hi\n")
        );
        assert_eq!(run_until(soon(), "false", &[]).await, None);
        assert_eq!(run_until(soon(), "/no/such/program", &[]).await, None);
        let started = Instant::now();
        assert_eq!(run_until(soon(), "sleep", &["5"]).await, None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
