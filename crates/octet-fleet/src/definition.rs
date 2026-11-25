//! The workflow file: YAML parsing into plain data types.
//!
//! Every struct rejects unknown keys, so a typo such as `approval_form:` is a
//! parse error naming the key rather than a silently ignored field. Semantic
//! checks (references, kinds, ordering) belong to validation, not here.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A workflow file could not be read into a [`Workflow`].
#[derive(Debug, thiserror::Error)]
pub enum DefinitionError {
    /// The YAML is malformed, misses a field, or has an unknown key.
    #[error("workflow YAML: {0}")]
    Yaml(String),
    /// A duration is not `<n>s`, `<n>m` or `<n>h` with `n >= 1`.
    #[error("invalid duration: {0}")]
    Duration(String),
}

/// A parsed workflow file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    /// Schema version; validation requires 1.
    pub version: u32,
    /// The workflow's name.
    pub name: String,
    /// Loop and retry limits.
    #[serde(default)]
    pub limits: Limits,
    /// Named vendor configurations.
    pub profiles: BTreeMap<String, Profile>,
    /// Named roles: a kind of work plus its instructions.
    pub roles: BTreeMap<String, Role>,
    /// The steps, in order.
    pub steps: Vec<StepDef>,
}

/// Loop and retry limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Backward jumps allowed per edge.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    /// Retries for transient vendor failures.
    #[serde(default = "default_transient_retries")]
    pub transient_retries: u32,
}

const fn default_max_rounds() -> u32 {
    5
}

const fn default_transient_retries() -> u32 {
    3
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rounds: default_max_rounds(),
            transient_retries: default_transient_retries(),
        }
    }
}

/// How to start one vendor.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// An engine name, as `Engine::parse` accepts.
    pub engine: String,
    /// Model name, if the engine takes one.
    #[serde(default)]
    pub model: Option<String>,
    /// Effort level, checked per engine at validation.
    #[serde(default)]
    pub effort: Option<String>,
    /// Permission mode (default `ask`).
    #[serde(default)]
    pub mode: Option<String>,
    /// Binary to run (default: the provider's default binary).
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// How long to wait on an approval, as a duration string.
    #[serde(default)]
    pub approval_timeout: Option<String>,
}

/// What a role's step produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A document (reply text): research, a plan.
    Document,
    /// A verdict on a document or a change.
    Verdict,
    /// A code change.
    Change,
    /// Tests run against a change.
    Tests,
}

/// A kind of work plus the instructions given to the vendor.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// What the role's steps produce.
    pub kind: Kind,
    /// The instructions text.
    pub instructions: String,
}

/// Whether a step starts a new vendor session or continues the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPolicy {
    /// Start a new session.
    Fresh,
    /// Continue the producing step's session.
    Continue,
}

/// A document a step builds on, reachable only through its approval.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRef {
    /// The document step.
    pub from: String,
    /// The verdict step that approves that document.
    pub approval_from: String,
}

/// One step of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepDef {
    /// Unique step name.
    pub id: String,
    /// The role this step plays.
    pub role: String,
    /// The profile it runs under.
    pub profile: String,
    /// For a verdict: the step whose output it judges.
    #[serde(default)]
    pub reviews: Option<String>,
    /// Approved documents the step builds on.
    #[serde(default)]
    pub inputs: Vec<InputRef>,
    /// For a tests step: the change step it tests.
    #[serde(default)]
    pub change_from: Option<String>,
    /// For a change review: the tests step whose evidence it reads.
    #[serde(default)]
    pub evidence_from: Option<String>,
    /// Test commands, for a tests step.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Time limit, as a duration string.
    #[serde(default)]
    pub timeout: Option<String>,
    /// Session policy override.
    #[serde(default)]
    pub session: Option<SessionPolicy>,
    /// Optional spelling of the changes-requested edge (must name the producer).
    #[serde(default)]
    pub on_changes_requested: Option<String>,
    /// Optional spelling of the tests-failed edge (must name the producer).
    #[serde(default)]
    pub on_tests_failed: Option<String>,
}

/// Parses a workflow file's YAML.
///
/// # Errors
///
/// [`DefinitionError::Yaml`] for malformed YAML, a missing field, or an
/// unknown key (the message names it).
pub fn parse(yaml: &str) -> Result<Workflow, DefinitionError> {
    serde_norway::from_str(yaml).map_err(|e| DefinitionError::Yaml(e.to_string()))
}

/// Parses `<n>s`, `<n>m` or `<n>h` with `n >= 1`.
///
/// # Errors
///
/// [`DefinitionError::Duration`] for any other text, or a zero count.
pub fn parse_duration(text: &str) -> Result<Duration, DefinitionError> {
    let bad = || DefinitionError::Duration(text.to_owned());
    let unit = match text.chars().last() {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 3600,
        _ => return Err(bad()),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let count: u64 = digits.parse().map_err(|_| bad())?;
    if count == 0 {
        return Err(bad());
    }
    count
        .checked_mul(unit)
        .map(Duration::from_secs)
        .ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SHIPPED: &str = r#"
version: 1
name: research-plan-build
limits:
  max_rounds: 5            # backward jumps per edge
  transient_retries: 3
profiles:
  claude-research: { engine: claude, model: claude-opus-5-5, effort: high }
  codex-review:    { engine: codex,  model: gpt-6-astra }
  codex-code:      { engine: codex,  model: gpt-6-astra, mode: full-access }
  opencode-test:   { engine: opencode, mode: accept-edits }
  claude-review:   { engine: claude, model: claude-opus-5-5 }
roles:
  researcher: { kind: document, instructions: "Investigate the code: what exists, what the task touches, risks …" }
  planner:    { kind: document, instructions: "Write an implementation plan from the approved research …" }
  reviewer:   { kind: verdict,  instructions: "Review against the task …" }
  coder:      { kind: change,   instructions: "Implement the approved plan …" }
  tester:     { kind: tests,    instructions: "Write or select tests for the change …" }
steps:
  - id: research
    role: researcher
    profile: claude-research
  - id: review_research
    role: reviewer
    profile: codex-review
    reviews: research          # rejection goes back to research (the producer)
  - id: plan
    role: planner
    profile: claude-research
    inputs:
      - { from: research, approval_from: review_research }
  - id: review_plan
    role: reviewer
    profile: codex-review
    reviews: plan              # rejection goes back to plan
  - id: implement
    role: coder
    profile: codex-code
    inputs:
      - { from: plan, approval_from: review_plan }
  - id: test
    role: tester
    profile: opencode-test
    change_from: implement     # failure goes back to implement (the producer)
    commands: ["cargo test --workspace"]
    timeout: 20m
  - id: review_change
    role: reviewer
    profile: claude-review
    reviews: implement         # rejection goes back to implement
    evidence_from: test
"#;

    #[test]
    fn the_shipped_shape_parses() {
        let workflow = parse(SHIPPED).unwrap();
        assert_eq!(workflow.version, 1);
        assert_eq!(workflow.name, "research-plan-build");
        assert_eq!(workflow.steps.len(), 7);
        assert_eq!(workflow.limits.max_rounds, 5);
        assert_eq!(workflow.profiles.len(), 5);
        assert_eq!(workflow.roles["coder"].kind, Kind::Change);
        assert_eq!(
            workflow.steps[2].inputs[0],
            InputRef {
                from: "research".into(),
                approval_from: "review_research".into()
            }
        );
        assert_eq!(workflow.steps[5].commands, ["cargo test --workspace"]);
        assert_eq!(workflow.steps[5].timeout.as_deref(), Some("20m"));
    }

    #[test]
    fn limits_default_to_five_and_three() {
        let workflow = parse("version: 1\nname: x\nprofiles: {}\nroles: {}\nsteps: []\n").unwrap();
        assert_eq!(
            workflow.limits,
            Limits {
                max_rounds: 5,
                transient_retries: 3
            }
        );
    }

    #[test]
    fn unknown_keys_are_errors() {
        let yaml = SHIPPED.replace(
            "approval_from: review_research",
            "approval_form: review_research",
        );
        match parse(&yaml) {
            Err(DefinitionError::Yaml(msg)) => assert!(msg.contains("approval_form"), "{msg}"),
            other => panic!("expected a Yaml error, got {other:?}"),
        }
    }

    #[test]
    fn a_typo_is_rejected_and_named_at_every_struct_level() {
        let cases: [(&str, &str, &str, &str); 6] = [
            (
                "Workflow",
                "steps:\n  - id: research",
                "nmae: x\nsteps:\n  - id: research",
                "nmae",
            ),
            (
                "Limits",
                "transient_retries: 3",
                "transient_retries: 3\n  max_round: 5",
                "max_round",
            ),
            (
                "Profile",
                "claude-review:   { engine: claude, model: claude-opus-5-5 }",
                "claude-review:   { engine: claude, modle: x }",
                "modle",
            ),
            (
                "Role",
                "tester:     { kind: tests,    instructions:",
                "tester:     { kind: tests, instrucions: x, instructions:",
                "instrucions",
            ),
            (
                "StepDef",
                "    profile: claude-research\n  - id: review_research",
                "    profile: claude-research\n    reviwes: x\n  - id: review_research",
                "reviwes",
            ),
            (
                "InputRef",
                "{ from: research, approval_from: review_research }",
                "{ from: research, approval_from: review_research, aproval_from: x }",
                "aproval_from",
            ),
        ];
        for (level, from, to, key) in cases {
            assert!(SHIPPED.contains(from), "{level}: fixture drifted");
            match parse(&SHIPPED.replacen(from, to, 1)) {
                Err(DefinitionError::Yaml(msg)) => {
                    assert!(msg.contains(key), "{level}: {msg}");
                }
                other => panic!("{level}: expected a Yaml error, got {other:?}"),
            }
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("20m").unwrap(), Duration::from_mins(20));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_hours(2));
        for bad in ["0m", "5", "m", "5d"] {
            assert!(
                matches!(parse_duration(bad), Err(DefinitionError::Duration(_))),
                "{bad}"
            );
        }
    }
}
