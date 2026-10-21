//! opencode launch configuration: its arguments, the agents that carry
//! Octet's permission modes, and the environment that starts it.
#![expect(dead_code, reason = "used by the protocol in the next task")]
use super::{Config, Mode};
use serde_json::{Value, value::RawValue};
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
    let (content, notice) = merged_content(user);
    let mut env: Vec<(OsString, OsString)> = vec![
        ("OPENCODE_CONFIG_CONTENT".into(), content.into()),
        ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()),
    ];
    if !config.plugins.load || !config.plugins.hooks || config.catalog_only {
        env.push(("OPENCODE_PURE".into(), "1".into()));
    }
    (env, notice)
}

/// A JSON object as its keys and values in source order. opencode applies a
/// permission map's rules last-match-wins in that order, so a merge must not
/// sort them; each value is kept as the text it was written in.
struct Ordered(Vec<(String, Box<RawValue>)>);

/// `text` without the leading whitespace JSON allows.
fn skip_space(text: &str) -> &str {
    text.trim_start_matches([' ', '\t', '\n', '\r'])
}

impl Ordered {
    /// The members of `text` in source order; `None` unless it is exactly
    /// one JSON object.
    fn parse(text: &str) -> Option<Ordered> {
        let mut ordered = Ordered(Vec::new());
        let mut rest = skip_space(skip_space(text).strip_prefix('{')?);
        if let Some(end) = rest.strip_prefix('}') {
            return skip_space(end).is_empty().then_some(ordered);
        }
        loop {
            let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            let key = keys.next()?.ok()?;
            rest = skip_space(&rest[keys.byte_offset()..]);
            rest = skip_space(rest.strip_prefix(':')?);
            let mut values = serde_json::Deserializer::from_str(rest).into_iter::<&RawValue>();
            let value = values.next()?.ok()?.to_owned();
            rest = skip_space(&rest[values.byte_offset()..]);
            ordered.set(key, value);
            if let Some(more) = rest.strip_prefix(',') {
                rest = skip_space(more);
            } else {
                return skip_space(rest.strip_prefix('}')?)
                    .is_empty()
                    .then_some(ordered);
            }
        }
    }

    /// Give `key` the value `value`: where `key` already is, or at the end.
    /// A repeated key keeps its first place and its last value, as when a
    /// parser builds a map.
    fn set(&mut self, key: String, value: Box<RawValue>) {
        match self.0.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key, value)),
        }
    }

    /// The value of `key`, if it is set.
    fn get(&self, key: &str) -> Option<&RawValue> {
        self.0
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| &**value)
    }

    /// The object as JSON text, in order, each value as it was.
    fn to_json(&self) -> String {
        let mut out = String::from("{");
        for (index, (key, value)) in self.0.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&Value::String(key.clone()).to_string());
            out.push(':');
            out.push_str(value.get());
        }
        out.push('}');
        out
    }

    /// This object as a value to put in another.
    fn into_raw(self) -> Box<RawValue> {
        raw(self.to_json())
    }
}

/// `json` as a raw value. Callers pass text they just built from valid JSON.
fn raw(json: String) -> Box<RawValue> {
    RawValue::from_string(json).expect("the text was built from valid JSON")
}

/// The `OPENCODE_CONFIG_CONTENT` text: the user's object with Octet's agents
/// set in its `agent` object, and the notice when `user` was not an object.
fn merged_content(user: Option<&str>) -> (String, Option<String>) {
    let (mut content, notice) = match user.map(Ordered::parse) {
        None => (Ordered(Vec::new()), None),
        Some(Some(object)) => (object, None),
        Some(None) => (Ordered(Vec::new()), Some(AGENT_NOTICE.to_owned())),
    };
    let mut agents = content
        .get("agent")
        .and_then(|agents| Ordered::parse(agents.get()))
        .unwrap_or(Ordered(Vec::new()));
    for (name, definition) in OCTET_AGENTS {
        agents.set((*name).to_owned(), raw((*definition).to_owned()));
    }
    content.set("agent".to_owned(), agents.into_raw());
    (content.to_json(), notice)
}

/// The agents Octet defines for its `ask`, `accept-edits` and `full-access`
/// modes, as JSON text. Reads are allowed everywhere, since they change
/// nothing. Each permission map starts with `"*"`: rules are last-match-wins,
/// so the entries after it refine it.
const OCTET_AGENTS: &[(&str, &str)] = &[
    (
        "octet-ask",
        r#"{"mode":"primary","description":"Octet: ask before edits and commands",
        "permission":{"*":"ask","read":"allow","glob":"allow","grep":"allow","list":"allow",
        "lsp":"allow","todoread":"allow","todowrite":"allow","skill":"allow"}}"#,
    ),
    (
        "octet-accept-edits",
        r#"{"mode":"primary","description":"Octet: edits proceed, other actions ask",
        "permission":{"*":"ask","edit":"allow","read":"allow","glob":"allow","grep":"allow",
        "list":"allow","lsp":"allow","todoread":"allow","todowrite":"allow","skill":"allow"}}"#,
    ),
    (
        "octet-full-access",
        r#"{"mode":"primary","description":"Octet: no permission checks",
        "permission":{"*":"allow"}}"#,
    ),
];

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

    #[test]
    fn the_users_key_order_survives_the_merge() {
        // opencode's rules are last-match-wins in object order, so a re-sort
        // would change what the user's own configuration decides.
        let user = r#"{"permission":{"rm *":"deny","*":"allow"},
            "agent":{"mine":{"permission":{"z":"ask","a":"allow"}}},"model":"x"}"#;
        let (env, notice) = launch_env_from(&config(), Some(user));
        assert_eq!(notice, None);
        let text = var(&env, "OPENCODE_CONFIG_CONTENT").unwrap();
        let before = |first: &str, second: &str| {
            let at = |needle: &str| {
                text.find(needle)
                    .unwrap_or_else(|| panic!("{needle} in {text}"))
            };
            assert!(at(first) < at(second), "{first} before {second} in {text}");
        };
        before(r#""rm *":"deny""#, r#""*":"allow""#);
        before(r#""z":"ask""#, r#""a":"allow""#);
        // The top-level keys keep their places too, and the agents are added.
        before(r#""permission""#, r#""agent""#);
        before(r#""agent""#, r#""model""#);
        let content: Value = serde_json::from_str(&text).unwrap();
        for name in [
            "octet-ask",
            "octet-accept-edits",
            "octet-full-access",
            "mine",
        ] {
            assert!(content["agent"][name].is_object(), "{name}");
        }
        assert_eq!(content["model"], "x");
    }

    #[test]
    fn octet_permission_maps_start_with_the_catch_all() {
        let (env, _) = launch_env_from(&config(), None);
        let text = var(&env, "OPENCODE_CONFIG_CONTENT").unwrap();
        // Last match wins, so a leading "*" is what lets the entries after it
        // refine it.
        assert_eq!(text.matches(r#""permission":{"*":""#).count(), 3, "{text}");
    }

    #[test]
    fn only_one_json_object_is_parsed_as_the_users_content() {
        let keys = |text: &str| {
            Ordered::parse(text).map(|o| {
                o.0.into_iter()
                    .map(|(k, v)| (k, v.get().to_owned()))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(keys(" { } "), Some(vec![]));
        assert_eq!(
            keys(r#" {"b" : [1, {"x":"}"}] , "a":"\"q\"" , "b":2 } "#),
            Some(vec![
                ("b".into(), "2".into()),
                ("a".into(), r#""\"q\"""#.into())
            ])
        );
        for bad in [
            "",
            "[1]",
            "{",
            r#"{"a":1,}"#,
            r#"{"a":1} x"#,
            r#"{"a" 1}"#,
            "{1:2}",
        ] {
            assert!(Ordered::parse(bad).is_none(), "{bad}");
        }
    }
}
