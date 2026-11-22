//! Static validation: turns a parsed [`Workflow`] into a [`Resolved`] one.
//!
//! Validation starts no vendor and runs no command. It checks the workflow's
//! shape (versions, names, references, kinds, order, edges, the approval gate
//! and the completion gate) and resolves every profile through a [`Host`].
//! Git, ownership and trust checks belong to the run, not to this module.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use octet_core::{DEFAULT_APPROVAL_TIMEOUT, Engine, Mode};
use serde::{Deserialize, Serialize};

use crate::definition::{
    InputRef, Kind, Limits, Profile, SessionPolicy, StepDef, Workflow, parse_duration,
};

/// The default time limit of a tests step, in seconds (`20m`).
const DEFAULT_TESTS_TIMEOUT_SECS: u64 = 20 * 60;

/// What validation needs to know about the machine and the vendors.
pub trait Host {
    /// Whether `engine` names a provider.
    fn engine_known(&self, engine: &str) -> bool;

    /// Whether `engine` takes `effort`.
    ///
    /// # Errors
    ///
    /// The reason it does not.
    fn check_effort(&self, engine: &str, effort: &str) -> Result<(), String>;

    /// The absolute binary to run for `engine`: `given` when the profile names
    /// one, else the provider's default. `None` when it cannot be found.
    fn resolve_binary(&self, engine: &str, given: Option<&Path>) -> Option<PathBuf>;
}

/// The real host: `octet_core`'s engines and the process's `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreHost;

impl Host for CoreHost {
    fn engine_known(&self, engine: &str) -> bool {
        Engine::parse(engine).is_some()
    }

    fn check_effort(&self, engine: &str, effort: &str) -> Result<(), String> {
        match Engine::parse(engine) {
            Some(engine) => engine.check_effort(effort),
            None => Err(format!("unknown engine {engine}")),
        }
    }

    fn resolve_binary(&self, engine: &str, given: Option<&Path>) -> Option<PathBuf> {
        let wanted = match given {
            Some(path) => path.to_path_buf(),
            None => PathBuf::from(Engine::parse(engine)?.provider().default_binary),
        };
        if wanted.components().count() > 1 || wanted.is_absolute() {
            let absolute = std::path::absolute(&wanted).ok()?;
            return is_executable(&absolute).then_some(absolute);
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(&wanted))
            .find(|candidate| is_executable(candidate))
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A workflow that passed validation, with every reference an index into
/// `steps`. Persisted to `run.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    /// The workflow's name.
    pub name: String,
    /// Loop and retry limits.
    pub limits: Limits,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

/// One validated step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Unique step name.
    pub id: String,
    /// What the step produces.
    pub kind: Kind,
    /// The role's instructions text.
    pub instructions: String,
    /// The vendor configuration, fully resolved.
    pub profile: ResolvedProfile,
    /// Whether the step starts a fresh session or continues the last.
    pub session: SessionPolicy,
    /// What the step reads, as indices into the workflow's steps.
    pub refs: Refs,
}

/// A step's references, by kind. Every index is an earlier step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Refs {
    /// A document step and the approved documents it builds on.
    Document {
        /// The approved documents it builds on.
        inputs: Vec<Input>,
    },
    /// A verdict on a document step.
    ReviewDocument {
        /// The reviewed document step.
        document: usize,
    },
    /// A change step and the approved documents it implements (at least one).
    Change {
        /// The approved documents it builds on.
        inputs: Vec<Input>,
    },
    /// Tests run against a change step.
    Tests {
        /// The change step under test.
        change: usize,
        /// The commands to run.
        commands: Vec<String>,
        /// The time limit for the commands, in seconds.
        timeout_secs: u64,
    },
    /// A verdict on a change step, reading a tests step's evidence.
    ReviewChange {
        /// The reviewed change step.
        change: usize,
        /// The tests step whose evidence it reads.
        evidence: usize,
    },
}

/// A document a step builds on, with the verdict step that approves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    /// The document step.
    pub document: usize,
    /// The verdict step reviewing that document.
    pub approval: usize,
}

/// A profile with every default filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    /// The engine name.
    pub engine: String,
    /// The absolute binary to run.
    pub binary: PathBuf,
    /// The model, if the profile names one.
    pub model: Option<String>,
    /// The effort level, if the profile names one.
    pub effort: Option<String>,
    /// The permission mode label (default `ask`).
    pub mode: String,
    /// How long to wait on an approval, in seconds.
    pub approval_timeout_secs: u64,
}

impl Step {
    /// Document and verdict steps never write; change and tests steps do.
    #[must_use]
    pub fn read_only(&self) -> bool {
        matches!(self.kind, Kind::Document | Kind::Verdict)
    }
}

impl Resolved {
    /// The step a negative outcome of `rejector` routes back to: a document
    /// review's document, a change review's or tests step's change. A step
    /// that cannot reject is its own producer.
    ///
    /// # Panics
    ///
    /// If `rejector` is not a step index.
    #[must_use]
    pub fn producer_of(&self, rejector: usize) -> usize {
        match &self.steps[rejector].refs {
            Refs::ReviewDocument { document } => *document,
            Refs::ReviewChange { change, .. } | Refs::Tests { change, .. } => *change,
            Refs::Document { .. } | Refs::Change { .. } => rejector,
        }
    }

    /// Every approval step `step` needs, directly or through its inputs'
    /// inputs, ascending and without duplicates.
    ///
    /// # Panics
    ///
    /// If `step` is not a step index.
    #[must_use]
    pub fn approvals_required(&self, step: usize) -> Vec<usize> {
        let mut approvals = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut pending = vec![step];
        while let Some(next) = pending.pop() {
            if !seen.insert(next) {
                continue;
            }
            if let Refs::Document { inputs } | Refs::Change { inputs } = &self.steps[next].refs {
                for input in inputs {
                    approvals.insert(input.approval);
                    pending.push(input.document);
                }
            }
        }
        approvals.into_iter().collect()
    }
}

/// A rule the workflow breaks. Each variant names the step (or profile) and
/// the key at fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// `version` is not 1.
    Version {
        /// The version found.
        found: u32,
    },
    /// A step ID is used twice.
    DuplicateStep {
        /// The repeated ID.
        id: String,
    },
    /// A step's `role` names no role.
    UnknownRole {
        /// The step.
        step: String,
        /// The missing role.
        role: String,
    },
    /// A step's `profile` names no profile.
    UnknownProfile {
        /// The step.
        step: String,
        /// The missing profile.
        profile: String,
    },
    /// A profile's engine is not one Octet knows.
    UnknownEngine {
        /// The profile.
        profile: String,
        /// The engine named.
        engine: String,
    },
    /// The engine does not take the profile's `effort`.
    BadEffort {
        /// The profile.
        profile: String,
        /// The effort named.
        effort: String,
        /// Why the engine refuses it.
        reason: String,
    },
    /// A profile's `mode` is not a permission mode.
    BadMode {
        /// The profile.
        profile: String,
        /// The mode named.
        mode: String,
    },
    /// A profile's binary cannot be found.
    BinaryNotFound {
        /// The profile.
        profile: String,
        /// The binary named, or the engine's default.
        binary: String,
    },
    /// A duration key is not `<n>s`, `<n>m` or `<n>h`.
    BadDuration {
        /// The profile or step.
        owner: String,
        /// The key.
        key: String,
        /// The text found.
        value: String,
    },
    /// A document or verdict step runs under a profile whose mode is not `ask`.
    ReadOnlyMode {
        /// The step.
        step: String,
        /// The profile.
        profile: String,
        /// The mode found.
        mode: String,
    },
    /// A key the step's kind requires is absent.
    Missing {
        /// The step.
        step: String,
        /// The key.
        key: String,
    },
    /// A key does not apply to the step's kind.
    NotApplicable {
        /// The step.
        step: String,
        /// The key.
        key: String,
        /// The step's kind.
        kind: Kind,
    },
    /// A reference names no step.
    UnknownReference {
        /// The step.
        step: String,
        /// The key.
        key: String,
        /// The name given.
        target: String,
    },
    /// A reference names the step itself or a later step.
    NotEarlier {
        /// The step.
        step: String,
        /// The key.
        key: String,
        /// The name given.
        target: String,
    },
    /// A reference names a step of the wrong kind.
    WrongKind {
        /// The step.
        step: String,
        /// The key.
        key: String,
        /// The name given.
        target: String,
        /// The kind the key needs.
        expected: &'static str,
    },
    /// An input's `approval_from` does not review the input's `from`.
    ApprovalMismatch {
        /// The step.
        step: String,
        /// The document named.
        from: String,
        /// The verdict step named.
        approval_from: String,
    },
    /// A change review's evidence tests a different change than it reviews.
    EvidenceMismatch {
        /// The step.
        step: String,
        /// The change it reviews.
        change: String,
        /// The tests step named as evidence.
        evidence: String,
    },
    /// A spelled edge does not name the step that produced the rejected artifact.
    EdgeNotProducer {
        /// The step.
        step: String,
        /// The edge key.
        key: String,
        /// The step it names.
        named: String,
        /// The producer it must name.
        producer: String,
    },
    /// The last step is not a change review with matching test evidence.
    CompletionGate {
        /// The last step, or empty when there are none.
        step: String,
        /// What is wrong.
        reason: &'static str,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version { found } => write!(f, "version: must be 1, found {found}"),
            Self::DuplicateStep { id } => write!(f, "step {id}: id is used more than once"),
            Self::UnknownRole { step, role } => {
                write!(f, "step {step}: role {role} is not defined")
            }
            Self::UnknownProfile { step, profile } => {
                write!(f, "step {step}: profile {profile} is not defined")
            }
            Self::UnknownEngine { profile, engine } => {
                write!(f, "profile {profile}: engine {engine} is not known")
            }
            Self::BadEffort {
                profile,
                effort,
                reason,
            } => write!(f, "profile {profile}: effort {effort}: {reason}"),
            Self::BadMode { profile, mode } => {
                write!(f, "profile {profile}: mode {mode} is not a mode")
            }
            Self::BinaryNotFound { profile, binary } => {
                write!(f, "profile {profile}: binary {binary} was not found")
            }
            Self::BadDuration { owner, key, value } => {
                write!(f, "{owner}: {key} {value:?} is not <n>s, <n>m or <n>h")
            }
            Self::ReadOnlyMode {
                step,
                profile,
                mode,
            } => write!(
                f,
                "step {step}: profile {profile} has mode {mode}, but a read-only step needs ask"
            ),
            Self::Missing { step, key } => write!(f, "step {step}: {key} is required"),
            Self::NotApplicable { step, key, kind } => {
                write!(f, "step {step}: {key} does not apply to a {kind:?} step")
            }
            Self::UnknownReference { step, key, target } => {
                write!(f, "step {step}: {key} names {target}, which is not a step")
            }
            Self::NotEarlier { step, key, target } => {
                write!(
                    f,
                    "step {step}: {key} names {target}, which does not come earlier"
                )
            }
            Self::WrongKind {
                step,
                key,
                target,
                expected,
            } => write!(
                f,
                "step {step}: {key} names {target}, which is not {expected}"
            ),
            Self::ApprovalMismatch {
                step,
                from,
                approval_from,
            } => write!(
                f,
                "step {step}: inputs: {approval_from} does not review {from}"
            ),
            Self::EvidenceMismatch {
                step,
                change,
                evidence,
            } => write!(
                f,
                "step {step}: evidence_from: {evidence} does not test {change}"
            ),
            Self::EdgeNotProducer {
                step,
                key,
                named,
                producer,
            } => write!(
                f,
                "step {step}: {key} names {named}, but the edge must name the producer {producer}"
            ),
            Self::CompletionGate { step, reason } => {
                if step.is_empty() {
                    write!(f, "steps: {reason}")
                } else {
                    write!(f, "step {step}: {reason}")
                }
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Checks `workflow` statically and resolves it.
///
/// # Errors
///
/// Every rule the workflow breaks, in one list (profiles first, then steps in
/// order, then the completion gate).
pub fn validate(workflow: &Workflow, host: &dyn Host) -> Result<Resolved, Vec<ValidationError>> {
    let mut errors = Vec::new();
    if workflow.version != 1 {
        errors.push(ValidationError::Version {
            found: workflow.version,
        });
    }
    let profiles: BTreeMap<&str, Option<ResolvedProfile>> = workflow
        .profiles
        .iter()
        .map(|(name, profile)| {
            (
                name.as_str(),
                resolve_profile(name, profile, host, &mut errors),
            )
        })
        .collect();
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, step) in workflow.steps.iter().enumerate() {
        if ids.insert(step.id.as_str(), index).is_some() {
            errors.push(ValidationError::DuplicateStep {
                id: step.id.clone(),
            });
        }
    }
    let kinds: Vec<Option<Kind>> = workflow
        .steps
        .iter()
        .map(|step| workflow.roles.get(&step.role).map(|role| role.kind))
        .collect();
    let context = Context {
        defs: &workflow.steps,
        ids: &ids,
        kinds: &kinds,
    };
    let mut steps = Vec::new();
    for (index, def) in workflow.steps.iter().enumerate() {
        let role = workflow.roles.get(&def.role);
        if role.is_none() {
            errors.push(ValidationError::UnknownRole {
                step: def.id.clone(),
                role: def.role.clone(),
            });
        }
        let profile = profiles.get(def.profile.as_str());
        if profile.is_none() {
            errors.push(ValidationError::UnknownProfile {
                step: def.id.clone(),
                profile: def.profile.clone(),
            });
        }
        let (Some(role), Some(Some(profile))) = (role, profile) else {
            // The failure was reported above (or with the profile itself);
            // references are still checked so one run shows every problem.
            if let Some(kind) = kinds[index] {
                let _ = context.refs(index, kind, &mut errors);
            }
            continue;
        };
        if matches!(role.kind, Kind::Document | Kind::Verdict) && profile.mode != "ask" {
            errors.push(ValidationError::ReadOnlyMode {
                step: def.id.clone(),
                profile: def.profile.clone(),
                mode: profile.mode.clone(),
            });
        }
        if let Some(refs) = context.refs(index, role.kind, &mut errors) {
            steps.push(Step {
                id: def.id.clone(),
                kind: role.kind,
                instructions: role.instructions.clone(),
                profile: profile.clone(),
                session: def.session.unwrap_or(match role.kind {
                    Kind::Document | Kind::Verdict => SessionPolicy::Fresh,
                    Kind::Change | Kind::Tests => SessionPolicy::Continue,
                }),
                refs,
            });
        }
    }
    completion_gate(&workflow.steps, &kinds, &mut errors);
    if errors.is_empty() {
        Ok(Resolved {
            name: workflow.name.clone(),
            limits: workflow.limits,
            steps,
        })
    } else {
        Err(errors)
    }
}

fn resolve_profile(
    name: &str,
    profile: &Profile,
    host: &dyn Host,
    errors: &mut Vec<ValidationError>,
) -> Option<ResolvedProfile> {
    let before = errors.len();
    if !host.engine_known(&profile.engine) {
        errors.push(ValidationError::UnknownEngine {
            profile: name.to_owned(),
            engine: profile.engine.clone(),
        });
        return None;
    }
    if let Some(effort) = &profile.effort
        && let Err(reason) = host.check_effort(&profile.engine, effort)
    {
        errors.push(ValidationError::BadEffort {
            profile: name.to_owned(),
            effort: effort.clone(),
            reason,
        });
    }
    let mode = profile.mode.as_deref().unwrap_or("ask");
    if Mode::parse(mode).is_none() {
        errors.push(ValidationError::BadMode {
            profile: name.to_owned(),
            mode: mode.to_owned(),
        });
    }
    let approval_timeout_secs = match &profile.approval_timeout {
        None => DEFAULT_APPROVAL_TIMEOUT.as_secs(),
        Some(text) => parse_duration(text).map_or_else(
            |_| {
                errors.push(ValidationError::BadDuration {
                    owner: format!("profile {name}"),
                    key: "approval_timeout".into(),
                    value: text.clone(),
                });
                0
            },
            |d| d.as_secs(),
        ),
    };
    let binary = host.resolve_binary(&profile.engine, profile.binary.as_deref());
    if binary.is_none() {
        errors.push(ValidationError::BinaryNotFound {
            profile: name.to_owned(),
            binary: profile
                .binary
                .as_ref()
                .map_or_else(|| profile.engine.clone(), |path| path.display().to_string()),
        });
    }
    if errors.len() > before {
        return None;
    }
    Some(ResolvedProfile {
        engine: profile.engine.clone(),
        binary: binary?,
        model: profile.model.clone(),
        effort: profile.effort.clone(),
        mode: mode.to_owned(),
        approval_timeout_secs,
    })
}

/// The step table that references are resolved against.
struct Context<'a> {
    defs: &'a [StepDef],
    ids: &'a BTreeMap<&'a str, usize>,
    kinds: &'a [Option<Kind>],
}

impl Context<'_> {
    /// Resolves the name a key gives to an earlier step of kind `expected`.
    fn lookup(
        &self,
        at: usize,
        key: &str,
        target: &str,
        wants: &[Kind],
        expected: &'static str,
        errors: &mut Vec<ValidationError>,
    ) -> Option<usize> {
        let step = self.defs[at].id.clone();
        let Some(&found) = self.ids.get(target) else {
            errors.push(ValidationError::UnknownReference {
                step,
                key: key.to_owned(),
                target: target.to_owned(),
            });
            return None;
        };
        if found >= at {
            errors.push(ValidationError::NotEarlier {
                step,
                key: key.to_owned(),
                target: target.to_owned(),
            });
            return None;
        }
        match self.kinds[found] {
            Some(kind) if wants.contains(&kind) => Some(found),
            // Its role is unknown; that is reported on that step.
            None => None,
            Some(_) => {
                errors.push(ValidationError::WrongKind {
                    step,
                    key: key.to_owned(),
                    target: target.to_owned(),
                    expected,
                });
                None
            }
        }
    }

    fn require<'v>(
        &self,
        at: usize,
        key: &str,
        value: Option<&'v String>,
        errors: &mut Vec<ValidationError>,
    ) -> Option<&'v str> {
        if value.is_none() {
            errors.push(ValidationError::Missing {
                step: self.defs[at].id.clone(),
                key: key.to_owned(),
            });
        }
        value.map(String::as_str)
    }

    /// Flags every key that does not belong to a step of `kind`, and (for a
    /// verdict) whichever of the two edge spellings does not match its target.
    fn misplaced(&self, at: usize, kind: Kind, keep: &[&str], errors: &mut Vec<ValidationError>) {
        let def = &self.defs[at];
        let present = [
            ("reviews", def.reviews.is_some()),
            ("inputs", !def.inputs.is_empty()),
            ("change_from", def.change_from.is_some()),
            ("evidence_from", def.evidence_from.is_some()),
            ("commands", !def.commands.is_empty()),
            ("timeout", def.timeout.is_some()),
            ("on_changes_requested", def.on_changes_requested.is_some()),
            ("on_tests_failed", def.on_tests_failed.is_some()),
        ];
        for (key, set) in present {
            if set && !keep.contains(&key) {
                errors.push(ValidationError::NotApplicable {
                    step: def.id.clone(),
                    key: key.to_owned(),
                    kind,
                });
            }
        }
    }

    fn edge(
        &self,
        at: usize,
        key: &str,
        named: Option<&String>,
        producer: Option<usize>,
        errors: &mut Vec<ValidationError>,
    ) {
        let (Some(named), Some(producer)) = (named, producer) else {
            return;
        };
        let producer = &self.defs[producer].id;
        if named != producer {
            errors.push(ValidationError::EdgeNotProducer {
                step: self.defs[at].id.clone(),
                key: key.to_owned(),
                named: named.clone(),
                producer: producer.clone(),
            });
        }
    }

    fn inputs(
        &self,
        at: usize,
        inputs: &[InputRef],
        errors: &mut Vec<ValidationError>,
    ) -> Option<Vec<Input>> {
        let mut resolved = Some(Vec::new());
        for input in inputs {
            let document = self.lookup(
                at,
                "inputs.from",
                &input.from,
                &[Kind::Document],
                "a document step",
                errors,
            );
            let approval = self.lookup(
                at,
                "inputs.approval_from",
                &input.approval_from,
                &[Kind::Verdict],
                "a verdict step",
                errors,
            );
            if let Some(&index) = self.ids.get(input.approval_from.as_str())
                && self.kinds[index] == Some(Kind::Verdict)
                && self.defs[index].reviews.as_deref() != Some(input.from.as_str())
            {
                errors.push(ValidationError::ApprovalMismatch {
                    step: self.defs[at].id.clone(),
                    from: input.from.clone(),
                    approval_from: input.approval_from.clone(),
                });
                resolved = None;
            }
            match (document, approval, resolved.as_mut()) {
                (Some(document), Some(approval), Some(list)) => {
                    list.push(Input { document, approval });
                }
                _ => resolved = None,
            }
        }
        resolved
    }

    fn refs(&self, at: usize, kind: Kind, errors: &mut Vec<ValidationError>) -> Option<Refs> {
        let def = &self.defs[at];
        match kind {
            Kind::Document => {
                self.misplaced(at, kind, &["inputs"], errors);
                Some(Refs::Document {
                    inputs: self.inputs(at, &def.inputs, errors)?,
                })
            }
            Kind::Change => {
                self.misplaced(at, kind, &["inputs"], errors);
                if def.inputs.is_empty() {
                    errors.push(ValidationError::Missing {
                        step: def.id.clone(),
                        key: "inputs".into(),
                    });
                    return None;
                }
                Some(Refs::Change {
                    inputs: self.inputs(at, &def.inputs, errors)?,
                })
            }
            Kind::Tests => self.tests(at, errors),
            Kind::Verdict => self.verdict(at, errors),
        }
    }

    fn tests(&self, at: usize, errors: &mut Vec<ValidationError>) -> Option<Refs> {
        let def = &self.defs[at];
        self.misplaced(
            at,
            Kind::Tests,
            &["change_from", "commands", "timeout", "on_tests_failed"],
            errors,
        );
        let named = self.require(at, "change_from", def.change_from.as_ref(), errors);
        let change = named.and_then(|name| {
            self.lookup(
                at,
                "change_from",
                name,
                &[Kind::Change],
                "a change step",
                errors,
            )
        });
        self.edge(
            at,
            "on_tests_failed",
            def.on_tests_failed.as_ref(),
            change,
            errors,
        );
        if def.commands.is_empty() {
            errors.push(ValidationError::Missing {
                step: def.id.clone(),
                key: "commands".into(),
            });
        }
        let timeout_secs = match &def.timeout {
            None => Some(DEFAULT_TESTS_TIMEOUT_SECS),
            Some(text) => {
                let parsed = parse_duration(text).ok().map(|d| d.as_secs());
                if parsed.is_none() {
                    errors.push(ValidationError::BadDuration {
                        owner: format!("step {}", def.id),
                        key: "timeout".into(),
                        value: text.clone(),
                    });
                }
                parsed
            }
        };
        if def.commands.is_empty() {
            return None;
        }
        Some(Refs::Tests {
            change: change?,
            commands: def.commands.clone(),
            timeout_secs: timeout_secs?,
        })
    }

    fn verdict(&self, at: usize, errors: &mut Vec<ValidationError>) -> Option<Refs> {
        let def = &self.defs[at];
        let named = self.require(at, "reviews", def.reviews.as_ref(), errors)?;
        let target = self.lookup(
            at,
            "reviews",
            named,
            &[Kind::Document, Kind::Change],
            "a document or change step",
            errors,
        );
        let reviewed = target.and_then(|index| self.kinds[index]);
        let keep: &[&str] = if reviewed == Some(Kind::Change) {
            &["reviews", "evidence_from", "on_changes_requested"]
        } else {
            &["reviews", "on_changes_requested"]
        };
        self.misplaced(at, Kind::Verdict, keep, errors);
        self.edge(
            at,
            "on_changes_requested",
            def.on_changes_requested.as_ref(),
            target,
            errors,
        );
        match reviewed? {
            Kind::Document => Some(Refs::ReviewDocument { document: target? }),
            Kind::Change => {
                let change = target?;
                let evidence_name =
                    self.require(at, "evidence_from", def.evidence_from.as_ref(), errors)?;
                let evidence = self.lookup(
                    at,
                    "evidence_from",
                    evidence_name,
                    &[Kind::Tests],
                    "a tests step",
                    errors,
                )?;
                if self.defs[evidence].change_from.as_deref() != Some(named) {
                    errors.push(ValidationError::EvidenceMismatch {
                        step: def.id.clone(),
                        change: named.to_owned(),
                        evidence: evidence_name.to_owned(),
                    });
                    return None;
                }
                Some(Refs::ReviewChange { change, evidence })
            }
            Kind::Verdict | Kind::Tests => None,
        }
    }
}

/// The last step must be a verdict on a change step (its evidence is checked
/// where every change review is).
fn completion_gate(defs: &[StepDef], kinds: &[Option<Kind>], errors: &mut Vec<ValidationError>) {
    let Some(last) = defs.last() else {
        errors.push(ValidationError::CompletionGate {
            step: String::new(),
            reason: "the workflow has no steps",
        });
        return;
    };
    let reviews_change = kinds.last().copied().flatten() == Some(Kind::Verdict)
        && last.reviews.as_deref().is_some_and(|name| {
            defs.iter()
                .position(|d| d.id == name)
                .is_some_and(|i| kinds[i] == Some(Kind::Change))
        });
    if !reviews_change {
        errors.push(ValidationError::CompletionGate {
            step: last.id.clone(),
            reason: "the last step must be a verdict reviewing a change step",
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{Kind, SessionPolicy, parse};

    const SHIPPED: &str = include_str!("../../../docs/fleet/research-plan-build.yaml");

    struct FakeHost;

    impl Host for FakeHost {
        fn engine_known(&self, engine: &str) -> bool {
            ["claude", "codex", "opencode", "antigravity"].contains(&engine)
        }
        fn check_effort(&self, _engine: &str, effort: &str) -> Result<(), String> {
            if effort == "bogus" {
                Err("no such effort".into())
            } else {
                Ok(())
            }
        }
        fn resolve_binary(&self, engine: &str, _given: Option<&Path>) -> Option<PathBuf> {
            Some(PathBuf::from(format!("/bin/{engine}")))
        }
    }

    struct NoBinaries;
    impl Host for NoBinaries {
        fn engine_known(&self, _: &str) -> bool {
            true
        }
        fn check_effort(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn resolve_binary(&self, _: &str, _: Option<&Path>) -> Option<PathBuf> {
            None
        }
    }

    const PREAMBLE: &str = r"
version: 1
name: custom
profiles:
  p: { engine: claude }
  r: { engine: codex }
  c: { engine: codex, mode: full-access }
roles:
  doc:    { kind: document, instructions: d }
  rev:    { kind: verdict,  instructions: v }
  code:   { kind: change,   instructions: c }
  tester: { kind: tests,    instructions: t }
steps:
";

    const MINIMAL_STEPS: &str = r"
  - { id: research, role: doc, profile: p }
  - { id: review, role: rev, profile: r, reviews: research }
  - { id: coder, role: code, profile: c, inputs: [{ from: research, approval_from: review }] }
  - { id: tests, role: tester, profile: c, change_from: coder, commands: [t] }
  - { id: final, role: rev, profile: r, reviews: coder, evidence_from: tests }
";

    fn check(yaml: &str) -> Result<Resolved, Vec<ValidationError>> {
        validate(&parse(yaml).unwrap(), &FakeHost)
    }

    fn custom(steps: &str) -> Result<Resolved, Vec<ValidationError>> {
        check(&format!("{PREAMBLE}{steps}"))
    }

    fn errors(result: Result<Resolved, Vec<ValidationError>>) -> Vec<ValidationError> {
        result.expect_err("expected validation errors")
    }

    fn replaced(from: &str, to: &str) -> String {
        assert!(SHIPPED.contains(from), "{from}");
        SHIPPED.replacen(from, to, 1)
    }

    fn index(resolved: &Resolved, id: &str) -> usize {
        resolved.steps.iter().position(|s| s.id == id).unwrap()
    }

    #[test]
    fn the_shipped_workflow_validates_and_gates_on_both_approvals() {
        let resolved = check(SHIPPED).unwrap();
        let implement = index(&resolved, "implement");
        let required = resolved.approvals_required(implement);
        assert!(required.contains(&index(&resolved, "review_plan")));
        assert!(required.contains(&index(&resolved, "review_research")));
        assert_eq!(required.len(), 2);
        assert_eq!(resolved.name, "research-plan-build");
        assert_eq!(resolved.steps.len(), 7);
        let profile = &resolved.steps[0].profile;
        assert_eq!(profile.binary, PathBuf::from("/bin/claude"));
        assert_eq!(profile.effort.as_deref(), Some("high"));
        assert_eq!(profile.mode, "ask");
        assert_eq!(
            profile.approval_timeout_secs,
            octet_core::DEFAULT_APPROVAL_TIMEOUT.as_secs()
        );
        assert_eq!(resolved.steps[implement].profile.mode, "full-access");
        assert_eq!(
            resolved.steps[index(&resolved, "test")].refs,
            Refs::Tests {
                change: implement,
                commands: vec!["cargo test --workspace".into()],
                timeout_secs: 1200
            }
        );
    }

    #[test]
    fn defaults_and_producers() {
        let resolved = check(SHIPPED).unwrap();
        let session = |id: &str| resolved.steps[index(&resolved, id)].session;
        assert_eq!(session("research"), SessionPolicy::Fresh);
        assert_eq!(session("review_plan"), SessionPolicy::Fresh);
        assert_eq!(session("implement"), SessionPolicy::Continue);
        assert_eq!(session("test"), SessionPolicy::Continue);
        let at = |id: &str| index(&resolved, id);
        assert_eq!(resolved.producer_of(at("review_research")), at("research"));
        assert_eq!(resolved.producer_of(at("review_plan")), at("plan"));
        assert_eq!(resolved.producer_of(at("test")), at("implement"));
        assert_eq!(resolved.producer_of(at("review_change")), at("implement"));
        assert!(resolved.steps[at("research")].read_only());
        assert!(resolved.steps[at("review_plan")].read_only());
        assert!(!resolved.steps[at("implement")].read_only());
        assert!(!resolved.steps[at("test")].read_only());
        assert_eq!(resolved.steps[at("implement")].kind, Kind::Change);
    }

    #[test]
    fn a_minimal_custom_workflow_is_valid_with_one_approval() {
        let resolved = custom(MINIMAL_STEPS).unwrap();
        assert_eq!(
            resolved.approvals_required(index(&resolved, "coder")),
            [index(&resolved, "review")]
        );
        assert!(
            resolved
                .approvals_required(index(&resolved, "research"))
                .is_empty()
        );
    }

    #[test]
    fn an_input_without_its_approval_is_rejected() {
        let yaml = format!(
            "{PREAMBLE}{}",
            MINIMAL_STEPS.replace(", approval_from: review", "")
        );
        assert!(parse(&yaml).is_err());
    }

    #[test]
    fn an_approval_that_reviews_another_document_is_rejected() {
        let found = errors(check(&replaced(
            "{ from: plan, approval_from: review_plan }",
            "{ from: plan, approval_from: review_research }",
        )));
        assert!(
            found.iter().any(|e| matches!(
                e,
                ValidationError::ApprovalMismatch { step, .. } if step == "implement"
            )),
            "{found:?}"
        );
    }

    #[test]
    fn references_must_be_earlier_and_of_the_right_kind() {
        let forward = errors(custom(
            r"
  - { id: research, role: doc, profile: p }
  - { id: review, role: rev, profile: r, reviews: research }
  - { id: tests, role: tester, profile: c, change_from: coder, commands: [t] }
  - { id: coder, role: code, profile: c, inputs: [{ from: research, approval_from: review }] }
  - { id: final, role: rev, profile: r, reviews: coder, evidence_from: tests }
",
        ));
        assert!(
            forward.iter().any(|e| matches!(
                e,
                ValidationError::NotEarlier { step, key, target }
                    if step == "tests" && key == "change_from" && target == "coder"
            )),
            "{forward:?}"
        );
        let wrong_kind = errors(custom(
            r"
  - { id: research, role: doc, profile: p }
  - { id: review, role: rev, profile: r, reviews: research }
  - { id: coder, role: code, profile: c, inputs: [{ from: research, approval_from: review }] }
  - { id: tests, role: tester, profile: c, change_from: coder, commands: [t] }
  - { id: odd, role: rev, profile: r, reviews: tests }
  - { id: final, role: rev, profile: r, reviews: coder, evidence_from: tests }
",
        ));
        assert!(
            wrong_kind.iter().any(|e| matches!(
                e,
                ValidationError::WrongKind { step, key, target, .. }
                    if step == "odd" && key == "reviews" && target == "tests"
            )),
            "{wrong_kind:?}"
        );
        let unknown = errors(custom(
            r"
  - { id: research, role: doc, profile: p }
  - { id: review, role: rev, profile: r, reviews: nowhere }
",
        ));
        assert!(unknown.iter().any(|e| matches!(
            e,
            ValidationError::UnknownReference { target, .. } if target == "nowhere"
        )));
    }

    #[test]
    fn edges_must_name_the_producer() {
        let found = errors(check(&replaced(
            "    evidence_from: test\n",
            "    evidence_from: test\n    on_changes_requested: research\n",
        )));
        assert!(
            found.iter().any(|e| matches!(
                e,
                ValidationError::EdgeNotProducer { step, key, named, producer }
                    if step == "review_change"
                        && key == "on_changes_requested"
                        && named == "research"
                        && producer == "implement"
            )),
            "{found:?}"
        );
        assert!(
            check(&replaced(
                "    evidence_from: test\n",
                "    evidence_from: test\n    on_changes_requested: implement\n",
            ))
            .is_ok()
        );
        let tests_edge = errors(check(&replaced(
            "    commands:",
            "    on_tests_failed: plan\n    commands:",
        )));
        assert!(
            tests_edge
                .iter()
                .any(|e| matches!(e, ValidationError::EdgeNotProducer { key, .. } if key == "on_tests_failed"))
        );
        assert!(
            check(&replaced(
                "    commands:",
                "    on_tests_failed: implement\n    commands:",
            ))
            .is_ok()
        );
    }

    #[test]
    fn the_completion_gate() {
        let document_last = errors(custom(
            r"
  - { id: research, role: doc, profile: p }
  - { id: review, role: rev, profile: r, reviews: research }
",
        ));
        assert!(
            document_last.iter().any(
                |e| matches!(e, ValidationError::CompletionGate { step, .. } if step == "review")
            ),
            "{document_last:?}"
        );
        assert!(
            errors(custom("  []\n"))
                .iter()
                .any(|e| matches!(e, ValidationError::CompletionGate { .. }))
        );
        let other_change = errors(custom(
            r"
  - { id: research, role: doc, profile: p }
  - { id: review, role: rev, profile: r, reviews: research }
  - { id: first, role: code, profile: c, inputs: [{ from: research, approval_from: review }] }
  - { id: tests, role: tester, profile: c, change_from: first, commands: [t] }
  - { id: second, role: code, profile: c, inputs: [{ from: research, approval_from: review }] }
  - { id: final, role: rev, profile: r, reviews: second, evidence_from: tests }
",
        ));
        assert!(
            other_change.iter().any(|e| matches!(
                e,
                ValidationError::EvidenceMismatch { step, .. } if step == "final"
            )),
            "{other_change:?}"
        );
    }

    #[test]
    fn read_only_profiles_must_ask() {
        let found = errors(check(&replaced(
            "claude-research: { engine: claude, model: claude-opus-5-5, effort: high }",
            "claude-research: { engine: claude, model: claude-opus-5-5, effort: high, mode: auto }",
        )));
        assert!(
            found.iter().any(|e| matches!(
                e,
                ValidationError::ReadOnlyMode { step, mode, .. }
                    if step == "research" && mode == "auto"
            )),
            "{found:?}"
        );
        // An explicit `ask` is fine.
        assert!(
            check(&replaced(
                "codex-review:    { engine: codex,  model: gpt-6-astra }",
                "codex-review:    { engine: codex,  model: gpt-6-astra, mode: ask }",
            ))
            .is_ok()
        );
    }

    #[test]
    fn tests_need_commands() {
        let found = errors(check(&replaced(
            "    commands: [\"cargo test --workspace\"]\n",
            "",
        )));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::Missing { step, key } if step == "test" && key == "commands"
        )));
    }

    #[test]
    fn unique_step_ids() {
        let found = errors(custom(&format!(
            "{MINIMAL_STEPS}  - {{ id: research, role: doc, profile: p }}\n"
        )));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::DuplicateStep { id } if id == "research"
        )));
    }

    #[test]
    fn unknown_engine() {
        let found = errors(check(&replaced("engine: opencode,", "engine: gemini,")));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::UnknownEngine { profile, engine }
                if profile == "opencode-test" && engine == "gemini"
        )));
    }

    #[test]
    fn bad_effort() {
        let found = errors(check(&replaced("effort: high", "effort: bogus")));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::BadEffort { profile, .. } if profile == "claude-research"
        )));
    }

    #[test]
    fn bad_mode() {
        let found = errors(check(&replaced("mode: accept-edits", "mode: yolo")));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::BadMode { profile, mode } if profile == "opencode-test" && mode == "yolo"
        )));
    }

    #[test]
    fn version_must_be_one() {
        let found = errors(check(&replaced("version: 1", "version: 2")));
        assert!(
            found
                .iter()
                .any(|e| matches!(e, ValidationError::Version { found: 2 }))
        );
    }

    #[test]
    fn durations_and_binaries_are_checked() {
        let found = errors(check(&replaced("timeout: 20m", "timeout: soon")));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::BadDuration { key, value, .. } if key == "timeout" && value == "soon"
        )));
        let found = errors(check(&replaced(
            "codex-review:    { engine: codex,  model: gpt-6-astra }",
            "codex-review:    { engine: codex,  approval_timeout: 0m }",
        )));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::BadDuration { key, .. } if key == "approval_timeout"
        )));
        let found = validate(&parse(SHIPPED).unwrap(), &NoBinaries).unwrap_err();
        assert_eq!(found.len(), 5, "one per profile: {found:?}");
        assert!(
            found
                .iter()
                .all(|e| matches!(e, ValidationError::BinaryNotFound { .. }))
        );
    }

    #[test]
    fn keys_that_do_not_apply_to_the_kind_are_rejected() {
        let found = errors(check(&replaced(
            "    reviews: research ",
            "    commands: [x]\n    reviews: research ",
        )));
        assert!(found.iter().any(|e| matches!(
            e,
            ValidationError::NotApplicable { step, key, .. }
                if step == "review_research" && key == "commands"
        )));
    }

    #[test]
    fn all_errors_are_reported_together() {
        let yaml = replaced("effort: high", "effort: bogus").replacen(
            "mode: accept-edits",
            "mode: yolo",
            1,
        );
        assert_eq!(errors(check(&yaml)).len(), 2);
    }

    #[test]
    fn errors_name_the_step_and_key() {
        let found = errors(check(&replaced(
            "    evidence_from: test\n",
            "    evidence_from: test\n    on_changes_requested: research\n",
        )));
        let text = found[0].to_string();
        assert!(
            text.contains("review_change") && text.contains("on_changes_requested"),
            "{text}"
        );
    }

    #[test]
    fn resolved_round_trips_through_json() {
        let resolved = check(SHIPPED).unwrap();
        let json = serde_json::to_string(&resolved).unwrap();
        assert_eq!(serde_json::from_str::<Resolved>(&json).unwrap(), resolved);
    }

    #[test]
    fn core_host_resolves_engines_efforts_and_binaries() {
        let host = CoreHost;
        assert!(host.engine_known("claude"));
        assert!(!host.engine_known("Claude"));
        assert!(host.check_effort("claude", "high").is_ok());
        assert!(host.check_effort("claude", "bogus").is_err());
        assert!(host.check_effort("nope", "high").is_err());
        assert_eq!(
            host.resolve_binary("claude", Some(Path::new("/bin/sh"))),
            Some(PathBuf::from("/bin/sh"))
        );
        assert_eq!(
            host.resolve_binary("claude", Some(Path::new("/no/such/binary"))),
            None
        );
        let found = host
            .resolve_binary("claude", Some(Path::new("sh")))
            .unwrap();
        assert!(found.is_absolute() && found.ends_with("sh"), "{found:?}");
        assert_eq!(host.resolve_binary("no-such-engine", None), None);
    }
}
