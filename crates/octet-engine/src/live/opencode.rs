//! opencode launch configuration: its arguments, the agents that carry
//! Octet's permission modes, and the environment that starts it.
#![expect(dead_code, reason = "used by the protocol in the next task")]
use super::{Config, Mode};
use serde_json::{Map, Value, json};
use std::ffi::OsString;

/// What Octet says when `OPENCODE_CONFIG_CONTENT` is not a JSON object and
/// has to be replaced.
pub(super) const AGENT_NOTICE: &str =
    "OPENCODE_CONFIG_CONTENT is not a JSON object; Octet replaced it for this session";

/// The agent that carries `mode`. `Auto` is opencode's own `build`, whose
/// permissions are the user's.
pub(super) fn agent(mode: Mode) -> &'static str {
    match mode {
        Mode::Ask => "octet-ask",
        Mode::AcceptEdits => "octet-accept-edits",
        Mode::Auto => "build",
        Mode::FullAccess => "octet-full-access",
    }
}

/// The mode `agent` carries; `None` for `plan` or an agent of the user's.
pub(super) fn mode_of(agent_name: &str) -> Option<Mode> {
    Mode::ALL
        .into_iter()
        .find(|mode| agent(*mode) == agent_name)
}

/// The arguments that start `opencode acp` in the workspace.
pub(super) fn launch_args(config: &Config) -> Vec<OsString> {
    vec![
        "acp".into(),
        "--cwd".into(),
        config.cwd.clone().into_os_string(),
    ]
}

/// The variables that start opencode, with the user's
/// `OPENCODE_CONFIG_CONTENT` read from the environment. The notice is for
/// the caller that can show one; this one drops it.
pub(super) fn launch_env(config: &Config) -> Vec<(OsString, OsString)> {
    let user = std::env::var("OPENCODE_CONFIG_CONTENT").ok();
    launch_env_from(config, user.as_deref()).0
}

/// The variables that start opencode, given the user's
/// `OPENCODE_CONFIG_CONTENT` as `user`, and a notice when that content was
/// not a JSON object and was replaced. Octet's three agents are set over the
/// user's `agent` object; everything else the user wrote is kept, including
/// a top-level `permission`, which `build` uses.
pub(super) fn launch_env_from(
    config: &Config,
    user: Option<&str>,
) -> (Vec<(OsString, OsString)>, Option<String>) {
    let (mut content, notice) = match user.map(serde_json::from_str::<Value>) {
        None => (Map::new(), None),
        Some(Ok(Value::Object(object))) => (object, None),
        Some(_) => (Map::new(), Some(AGENT_NOTICE.to_owned())),
    };
    let mut agents = match content.remove("agent") {
        Some(Value::Object(agents)) => agents,
        _ => Map::new(),
    };
    if let Value::Object(octet) = octet_agents() {
        agents.extend(octet);
    }
    content.insert("agent".into(), Value::Object(agents));
    let mut env: Vec<(OsString, OsString)> = vec![
        (
            "OPENCODE_CONFIG_CONTENT".into(),
            Value::Object(content).to_string().into(),
        ),
        ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()),
    ];
    if !config.plugins.load || !config.plugins.hooks || config.catalog_only {
        env.push(("OPENCODE_PURE".into(), "1".into()));
    }
    (env, notice)
}

/// The agents Octet defines for its `ask`, `accept-edits` and `full-access`
/// modes. Reads are allowed everywhere, since they change nothing.
fn octet_agents() -> Value {
    json!({
        "octet-ask": {
            "mode": "primary",
            "description": "Octet: ask before edits and commands",
            "permission": {"*": "ask", "read": "allow", "glob": "allow", "grep": "allow",
                "list": "allow", "lsp": "allow", "todoread": "allow", "todowrite": "allow",
                "skill": "allow"}
        },
        "octet-accept-edits": {
            "mode": "primary",
            "description": "Octet: edits proceed, other actions ask",
            "permission": {"*": "ask", "edit": "allow", "read": "allow", "glob": "allow",
                "grep": "allow", "list": "allow", "lsp": "allow", "todoread": "allow",
                "todowrite": "allow", "skill": "allow"}
        },
        "octet-full-access": {
            "mode": "primary",
            "description": "Octet: no permission checks",
            "permission": {"*": "allow"}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{Config, Engine, Mode, Plugins};
    use serde_json::{Value, json};
    use std::ffi::OsString;

    /// A configuration in `/w` with the default switches.
    fn config() -> Config {
        Config::new(Engine::CLAUDE, "opencode", "/w")
    }

    /// The value of `name` in `env`, if it is set.
    fn var(env: &[(OsString, OsString)], name: &str) -> Option<String> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    #[test]
    fn every_mode_has_an_agent_and_back() {
        for mode in Mode::ALL {
            assert_eq!(mode_of(agent(mode)), Some(mode));
        }
        assert_eq!(mode_of("plan"), None);
        assert_eq!(mode_of("mine"), None);
    }

    #[test]
    fn the_launch_is_acp_in_the_workspace() {
        assert_eq!(launch_args(&config()), ["acp", "--cwd", "/w"]);
    }

    #[test]
    fn octet_agents_win_over_the_users_and_the_rest_is_kept() {
        let user = r#"{"agent":{"octet-ask":{"permission":{"*":"allow"}},"mine":{}},
            "model":"x","permission":{"bash":"allow"}}"#;
        let (env, notice) = launch_env_from(&config(), Some(user));
        assert_eq!(notice, None);
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(content["agent"]["octet-ask"]["permission"]["*"], "ask");
        assert_eq!(
            content["agent"]["octet-full-access"]["permission"]["*"],
            "allow"
        );
        assert_eq!(
            content["agent"]["octet-accept-edits"]["permission"]["edit"],
            "allow"
        );
        assert_eq!(content["agent"]["mine"], json!({}));
        assert_eq!(content["model"], "x");
        assert_eq!(content["permission"]["bash"], "allow");
    }

    #[test]
    fn content_that_is_not_an_object_is_replaced_with_a_notice() {
        let (env, notice) = launch_env_from(&config(), Some("[1]"));
        assert_eq!(notice.as_deref(), Some(AGENT_NOTICE));
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(
            content.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["agent"]
        );
        // Content that is not JSON at all is replaced the same way.
        let (_, notice) = launch_env_from(&config(), Some("{oops"));
        assert_eq!(notice.as_deref(), Some(AGENT_NOTICE));
    }

    #[test]
    fn absent_content_gives_the_agents_alone_and_no_notice() {
        let (env, notice) = launch_env_from(&config(), None);
        assert_eq!(notice, None);
        let content: Value =
            serde_json::from_str(&var(&env, "OPENCODE_CONFIG_CONTENT").unwrap()).unwrap();
        assert_eq!(
            content.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["agent"]
        );
        assert_eq!(content["agent"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn project_config_is_always_off_and_plugins_follow_the_switches() {
        let pure = |config: &Config| var(&launch_env_from(config, None).0, "OPENCODE_PURE");
        let disabled = |config: &Config| {
            var(
                &launch_env_from(config, None).0,
                "OPENCODE_DISABLE_PROJECT_CONFIG",
            )
        };
        let mut c = config();
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), None);
        c.plugins = Plugins {
            load: false,
            hooks: true,
        };
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), Some("1".into()));
        c.plugins = Plugins {
            load: true,
            hooks: false,
        };
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), Some("1".into()));
        c.plugins = Plugins::default();
        c.catalog_only = true;
        assert_eq!(disabled(&c), Some("1".into()));
        assert_eq!(pure(&c), Some("1".into()));
    }
}
