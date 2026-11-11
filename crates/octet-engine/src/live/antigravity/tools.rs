//! The running turn's tool calls, as Antigravity's frames name them: each
//! call's name, target and kind, whether a permission request named it, and
//! the loosest mode in force while it ran. A call that ends unasked is judged
//! by the tripwire (see [`tripped`]); a call Octet cannot remember while the
//! mode asks stops the turn; a subagent is reported once a turn.
use crate::live::{Mode, ToolCall, ToolPhase, acp, activity::one_line};
use serde_json::Value;
use std::collections::HashMap;

/// The most tool calls one turn remembers.
const MAX_TOOLS: usize = 1024;

/// The name of the tool that starts one of Antigravity's subagents.
const SUBAGENT_TOOL: &str = "invoke_subagent";

/// What Octet says when Antigravity starts a subagent while the mode asks.
pub(super) const SUBAGENT_NOTICE: &str = concat!(
    "Antigravity started a subagent. Its approval requests never reach Octet, so what it must ask for ",
    "waits and does not run. In auto, Antigravity's own checker decides"
);

/// A tool call of the running turn, as its first frame named it.
struct Tool {
    /// The tool's name, else its title.
    name: String,
    /// The newest target a frame named.
    target: String,
    /// The strictest kind a frame named (see `Tool::update`); later frames
    /// need not repeat it.
    kind: String,
    /// The newest title a frame named: a command's target when no frame
    /// names one.
    title: Option<String>,
    /// A permission request named the call this turn: a call that ends
    /// without one ran unasked.
    asked: bool,
    /// The loosest mode in force while the call ran: the mode when its
    /// first frame came, raised by each switch confirmed before it ended.
    loosest: Mode,
    /// A frame said it `completed` or `failed`: it has been judged.
    ended: bool,
}

/// Why a tool frame stops the turn.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Stop {
    /// A call ran unasked where its mode asks: the tripwire's text.
    Unasked(String),
    /// A call Octet could not remember while the mode asks.
    TooMany,
}

impl Stop {
    /// Why the turn ends, in words to show.
    pub(super) fn reason(self) -> String {
        match self {
            Stop::Unasked(reason) => reason,
            Stop::TooMany => format!(
                "Antigravity made more than {MAX_TOOLS} tool calls in one turn, more than Octet checks for \
                 unasked actions. Octet cancelled the turn"
            ),
        }
    }
}

/// This turn's tool calls, by ID, at most [`MAX_TOOLS`], and whether its
/// subagent was reported.
#[derive(Default)]
pub(super) struct Tools {
    /// The calls, by ID.
    calls: HashMap<String, Tool>,
    /// A subagent was reported this turn.
    subagent_noted: bool,
}

impl Tools {
    /// Forgets every call: a turn starts or ends.
    pub(super) fn clear(&mut self) {
        self.calls.clear();
        self.subagent_noted = false;
    }

    /// The target `tool_call`'s earlier frames named this turn, if any.
    pub(super) fn known_target(&self, tool_call: &Value) -> Option<String> {
        tool_call["toolCallId"]
            .as_str()
            .and_then(|id| self.calls.get(&one_line(id)))
            .map(|tool| tool.target.clone())
            .filter(|target| !target.is_empty())
    }

    /// Marks the call `tool_call` names as asked, adding it to the table as
    /// the request names it, under `mode`, when it is not there yet. A call
    /// without an ID cannot be named again, and a full table takes no more.
    pub(super) fn mark_asked(&mut self, tool_call: &Value, mode: Mode) {
        let id = one_line(tool_call["toolCallId"].as_str().unwrap_or_default());
        if id.is_empty() {
            return;
        }
        if let Some(tool) = self.calls.get_mut(&id) {
            tool.asked = true;
        } else if self.calls.len() < MAX_TOOLS {
            let mut tool = Tool::first(tool_call, acp::frame_target(tool_call), mode);
            tool.asked = true;
            self.calls.insert(id, tool);
        }
    }

    /// A switch to `mode` was confirmed: every call still running ran under
    /// it too, if it is looser.
    pub(super) fn raise(&mut self, mode: Mode) {
        for tool in self.calls.values_mut().filter(|tool| !tool.ended) {
            if is_looser(mode, tool.loosest) {
                tool.loosest = mode;
            }
        }
    }

    /// A `tool_call` or `tool_call_update`, arriving under `mode`: the call
    /// as it now stands, and why it stops the turn, if it does. Its name is
    /// its first frame's; its kind is the strictest one a frame names (see
    /// `Tool::update`); its target is the newest one a frame names (see
    /// `acp::frame_target`), else a command's best title. At most
    /// [`MAX_TOOLS`] calls are remembered in a turn; while `mode` asks, a
    /// call that cannot be is a [`Stop::TooMany`]. A frame that ends a call
    /// no permission request named is judged by the tripwire; a call without
    /// an ID is judged on its own frame.
    pub(super) fn frame(&mut self, update: &Value, mode: Mode) -> (ToolCall, Option<Stop>) {
        let id = one_line(update["toolCallId"].as_str().unwrap_or_default());
        let target = acp::frame_target(update);
        let phase = acp::tool_phase(&update["status"]);
        let ends = phase != ToolPhase::Running;
        let (name, target, stop) = if let Some(known) = self.calls.get_mut(&id) {
            known.update(update, target);
            let stop = known.end(ends);
            (known.name.clone(), known.target.clone(), stop)
        } else {
            let mut tool = Tool::first(update, target, mode);
            let mut stop = tool.end(ends);
            let shown = (tool.name.clone(), tool.target.clone());
            if !id.is_empty() {
                if self.calls.len() < MAX_TOOLS {
                    self.calls.insert(id.clone(), tool);
                } else if asks(mode) {
                    stop = stop.or(Some(Stop::TooMany));
                }
            }
            (shown.0, shown.1, stop)
        };
        let call = ToolCall {
            id,
            name,
            target,
            phase,
            detail: acp::content_text(&update["content"]),
        };
        (call, stop)
    }

    /// Whether the call named `name`, arriving under `mode`, is the turn's
    /// first subagent while the mode asks: it is reported once a turn.
    pub(super) fn subagent_started(&mut self, name: &str, mode: Mode) -> bool {
        if name != SUBAGENT_TOOL || !asks(mode) || self.subagent_noted {
            return false;
        }
        self.subagent_noted = true;
        true
    }
}

impl Tool {
    /// A call as its first frame `update` names it, with the `target` that
    /// frame names, if any, arriving under `mode`.
    fn first(update: &Value, target: Option<String>, mode: Mode) -> Self {
        let named = |key: &str| {
            update[key]
                .as_str()
                .map(one_line)
                .filter(|text| !text.is_empty())
        };
        let kind = named("kind").unwrap_or_else(|| "other".to_owned());
        let title = named("title");
        let target = target
            .or_else(|| title.clone().filter(|_| kind == "execute"))
            .unwrap_or_default();
        Self {
            name: named("name")
                .or_else(|| title.clone())
                .unwrap_or_else(|| "tool".to_owned()),
            target,
            kind,
            title,
            asked: false,
            loosest: mode,
            ended: false,
        }
    }

    /// A later frame `update` of the call, naming `target`, if any. The
    /// strictest kind named so far judges the call (see [`strictness`]): a
    /// frame's kind replaces the one known only when it is stricter, or when
    /// both are kinds the tripwire never covers, so a later frame cannot
    /// make a command look like an edit or a read. A command that has no
    /// target yet takes the best title seen (the newest a frame named).
    fn update(&mut self, update: &Value, target: Option<String>) {
        let named = |key: &str| {
            update[key]
                .as_str()
                .map(one_line)
                .filter(|text| !text.is_empty())
        };
        if let Some(kind) = named("kind") {
            let known = strictness(&self.kind);
            if strictness(&kind) > known || known == 0 {
                self.kind = kind;
            }
        }
        if let Some(title) = named("title") {
            self.title = Some(title);
        }
        if let Some(target) = target {
            self.target = target;
        } else if self.target.is_empty()
            && self.kind == "execute"
            && let Some(title) = &self.title
        {
            self.target.clone_from(title);
        }
    }

    /// The call's frame says it `ends`: the first time, it is judged, and an
    /// unasked call the tripwire covers is a [`Stop::Unasked`].
    fn end(&mut self, ends: bool) -> Option<Stop> {
        if !ends || self.ended {
            return None;
        }
        self.ended = true;
        if self.asked {
            return None;
        }
        let what = tripped(&self.kind, self.loosest)?;
        let shown = if self.target.is_empty() {
            &self.name
        } else {
            &self.target
        };
        Some(Stop::Unasked(format!(
            "Antigravity ran {what} without asking while Octet shows {}: {shown}. Octet's settings did not take \
             effect with this agy, so ask and accept-edits cannot be relied on. Octet cancelled the turn and sends \
             no more prompts in this session. What ran cannot be undone",
            self.loosest.label()
        )))
    }
}

/// How strict the tripwire is about a call of `kind`: a command (2) asks in
/// ask and accept-edits, an edit (1) in ask, anything else (0) never.
fn strictness(kind: &str) -> u8 {
    match kind {
        "execute" => 2,
        "edit" | "delete" | "move" => 1,
        _ => 0,
    }
}

/// The tripwire: what a call of `kind` that ran unasked did, when `loosest`,
/// the loosest mode in force while it ran, should have asked for it — a
/// command in ask and accept-edits, an edit (`edit`, `delete`, `move`) in
/// ask; `None` for anything else.
pub(super) fn tripped(kind: &str, loosest: Mode) -> Option<&'static str> {
    match (kind, loosest) {
        ("execute", Mode::Ask | Mode::AcceptEdits) => Some("a command"),
        ("edit" | "delete" | "move", Mode::Ask) => Some("an edit"),
        _ => None,
    }
}

/// Whether `mode` asks before commands: ask and accept-edits.
fn asks(mode: Mode) -> bool {
    matches!(mode, Mode::Ask | Mode::AcceptEdits)
}

/// Whether `mode` lets more run unasked than `than`, in the order ask,
/// accept-edits, auto, full access.
pub(super) fn is_looser(mode: Mode, than: Mode) -> bool {
    let rank = |mode: Mode| match mode {
        Mode::Ask => 0,
        Mode::AcceptEdits => 1,
        Mode::Auto => 2,
        Mode::FullAccess => 3,
    };
    rank(mode) > rank(than)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tool_is_named_by_its_name_then_its_title() {
        let tool = Tool::first(
            &json!({"name": "run_command", "kind": "execute", "title": "ls -la"}),
            None,
            Mode::Ask,
        );
        assert_eq!(
            (tool.name.as_str(), tool.target.as_str()),
            ("run_command", "ls -la")
        );
        assert_eq!(tool.kind, "execute");
        let tool = Tool::first(
            &json!({"kind": "read", "title": "Read file"}),
            None,
            Mode::Ask,
        );
        assert_eq!(
            (tool.name.as_str(), tool.target.as_str()),
            ("Read file", "")
        );
        let tool = Tool::first(&json!({}), Some("/w/a".into()), Mode::Ask);
        assert_eq!((tool.name.as_str(), tool.target.as_str()), ("tool", "/w/a"));
        assert_eq!(tool.kind, "other");
    }

    #[test]
    fn the_tripwire_covers_commands_in_both_asking_modes_and_edits_in_ask() {
        for (kind, ask, accept_edits) in [
            ("execute", Some("a command"), Some("a command")),
            ("edit", Some("an edit"), None),
            ("delete", Some("an edit"), None),
            ("move", Some("an edit"), None),
            ("read", None, None),
            ("other", None, None),
            ("think", None, None),
        ] {
            assert_eq!(tripped(kind, Mode::Ask), ask, "{kind} in ask");
            assert_eq!(
                tripped(kind, Mode::AcceptEdits),
                accept_edits,
                "{kind} in accept-edits"
            );
            for mode in [Mode::Auto, Mode::FullAccess] {
                assert_eq!(tripped(kind, mode), None, "{kind} in {mode:?}");
            }
        }
    }

    #[test]
    fn a_confirmed_switch_raises_only_the_calls_still_running() {
        let mut tools = Tools::default();
        let command = |id: &str, status: &str| {
            json!({"toolCallId": id, "name": "run_command", "kind": "execute",
                "rawInput": {"command": "ls"}, "status": status})
        };
        let (_, stop) = tools.frame(&command("done", "completed"), Mode::Auto);
        assert_eq!(stop, None);
        tools.frame(&command("running", "in_progress"), Mode::Ask);
        tools.raise(Mode::Auto);
        // A tighter switch lowers nothing.
        tools.raise(Mode::Ask);
        assert_eq!(tools.calls["done"].loosest, Mode::Auto);
        assert_eq!(tools.calls["running"].loosest, Mode::Auto);
        let (_, stop) = tools.frame(&command("running", "completed"), Mode::Ask);
        assert_eq!(stop, None, "it ran while auto was in force");
        // A call begun under ask and ended unasked trips, once.
        tools.frame(&command("late", "in_progress"), Mode::Ask);
        let (call, stop) = tools.frame(&command("late", "failed"), Mode::Ask);
        assert_eq!(call.phase, ToolPhase::Failed);
        assert!(
            matches!(&stop, Some(Stop::Unasked(reason)) if reason.starts_with(
                "Antigravity ran a command without asking while Octet shows ask: ls."
            )),
            "{stop:?}"
        );
        assert_eq!(tools.frame(&command("late", "failed"), Mode::Ask).1, None);
    }

    #[test]
    fn a_kind_named_on_a_later_frame_is_judged_and_never_lowered() {
        let unasked = |stop: Option<Stop>, target: &str| {
            matches!(&stop, Some(Stop::Unasked(reason)) if reason.starts_with(&format!(
                "Antigravity ran a command without asking while Octet shows ask: {target}."
            )))
        };
        let completed = json!({"toolCallId": "c1", "kind": "execute",
            "rawInput": {"command": "rm x"}, "status": "completed"});
        // No kind at first: `other` until a later frame names one.
        let mut tools = Tools::default();
        tools.frame(
            &json!({"toolCallId": "c1", "title": "Run", "status": "pending"}),
            Mode::Ask,
        );
        let (call, stop) = tools.frame(&completed, Mode::Ask);
        assert_eq!(call.target, "rm x");
        assert!(unasked(stop, "rm x"));
        // A kind the tripwire does not cover gives way to one it does.
        let mut tools = Tools::default();
        tools.frame(
            &json!({"toolCallId": "c1", "kind": "read", "status": "in_progress"}),
            Mode::Ask,
        );
        assert!(unasked(tools.frame(&completed, Mode::Ask).1, "rm x"));
        // A command is not lowered by a later kind; a later frame's
        // title is the target of a command that named none.
        let mut tools = Tools::default();
        tools.frame(
            &json!({"toolCallId": "c1", "kind": "execute", "status": "pending"}),
            Mode::Ask,
        );
        let (_, stop) = tools.frame(
            &json!({"toolCallId": "c1", "kind": "read", "title": "ls", "status": "completed"}),
            Mode::Ask,
        );
        assert!(unasked(stop, "ls"));
    }

    #[test]
    fn the_strictest_kind_a_frame_names_judges_the_call() {
        // The kind the call is judged by, after frames naming `kinds` in order,
        // and the tripwire's verdict under `mode`.
        let judged = |kinds: &[&str], mode: Mode| {
            let mut tools = Tools::default();
            let (last, first) = kinds.split_last().unwrap();
            for kind in first {
                tools.frame(
                    &json!({"toolCallId": "c1", "kind": kind, "title": "t", "status": "in_progress"}),
                    mode,
                );
            }
            let (_, stop) = tools.frame(
                &json!({"toolCallId": "c1", "kind": last, "status": "completed"}),
                mode,
            );
            (tools.calls["c1"].kind.clone(), stop)
        };
        let ran = |stop: &Option<Stop>, what: &str, label: &str| {
            matches!(stop, Some(Stop::Unasked(reason))
                if reason.starts_with(&format!("Antigravity ran {what} without asking while Octet shows {label}")))
        };
        // An edit that turns out to be a command is a command: in
        // accept-edits it trips.
        let (kind, stop) = judged(&["edit", "execute"], Mode::AcceptEdits);
        assert_eq!(kind, "execute");
        assert!(ran(&stop, "a command", "accept-edits"), "{stop:?}");
        // A command stays a command; an edit stays an edit.
        let (kind, stop) = judged(&["execute", "edit"], Mode::Ask);
        assert_eq!(kind, "execute");
        assert!(ran(&stop, "a command", "ask"), "{stop:?}");
        let (kind, stop) = judged(&["execute", "read"], Mode::AcceptEdits);
        assert_eq!(kind, "execute");
        assert!(ran(&stop, "a command", "accept-edits"), "{stop:?}");
        let (kind, stop) = judged(&["edit", "read"], Mode::Ask);
        assert_eq!(kind, "edit");
        assert!(ran(&stop, "an edit", "ask"), "{stop:?}");
        // Among kinds the tripwire never covers, the newest is kept.
        assert_eq!(judged(&["read", "think"], Mode::Ask).0, "think");
    }

    #[test]
    fn a_command_without_a_target_falls_back_to_the_best_title_seen() {
        let mut tools = Tools::default();
        tools.frame(
            &json!({"toolCallId": "c1", "name": "run_command", "title": "ls -la", "status": "pending"}),
            Mode::Ask,
        );
        let (call, stop) = tools.frame(
            &json!({"toolCallId": "c1", "kind": "execute", "status": "completed"}),
            Mode::Ask,
        );
        assert_eq!(call.target, "ls -la");
        assert!(
            matches!(&stop, Some(Stop::Unasked(reason)) if reason.contains("shows ask: ls -la.")),
            "{stop:?}"
        );
    }

    #[test]
    fn a_call_octet_cannot_remember_stops_the_turn_only_while_the_mode_asks() {
        let mut tools = Tools::default();
        let read = |id: String| json!({"toolCallId": id, "kind": "read", "status": "in_progress"});
        for n in 0..MAX_TOOLS {
            assert_eq!(tools.frame(&read(n.to_string()), Mode::Ask).1, None);
        }
        assert_eq!(
            tools.frame(&read("over".to_owned()), Mode::Ask).1,
            Some(Stop::TooMany)
        );
        assert_eq!(
            tools.frame(&read("over".to_owned()), Mode::AcceptEdits).1,
            Some(Stop::TooMany)
        );
        assert_eq!(tools.frame(&read("over".to_owned()), Mode::Auto).1, None);
        assert_eq!(tools.calls.len(), MAX_TOOLS);
        assert!(
            Stop::TooMany
                .reason()
                .starts_with("Antigravity made more than 1024 tool calls in one turn")
        );
    }

    #[test]
    fn a_permission_request_marks_its_tool_call_asked() {
        let mut tools = Tools::default();
        let call = |id: &str| json!({"toolCallId": id, "name": "run_command", "kind": "execute", "title": "ls"});
        tools.calls.insert(
            "known".to_owned(),
            Tool::first(&call("known"), None, Mode::Ask),
        );
        tools.mark_asked(&call("known"), Mode::Ask);
        tools.mark_asked(&call("new"), Mode::Ask);
        // A call with no ID cannot be named again.
        tools.mark_asked(&json!({"kind": "execute"}), Mode::Ask);
        assert!(tools.calls["known"].asked);
        let added = &tools.calls["new"];
        assert!(added.asked);
        assert_eq!(
            (added.name.as_str(), added.target.as_str()),
            ("run_command", "ls")
        );
        assert_eq!(tools.calls.len(), 2);
        // A full table takes no more calls.
        for n in tools.calls.len()..MAX_TOOLS {
            tools
                .calls
                .insert(n.to_string(), Tool::first(&call("x"), None, Mode::Ask));
        }
        tools.mark_asked(&call("late"), Mode::Ask);
        assert!(!tools.calls.contains_key("late"));
        assert_eq!(tools.calls.len(), MAX_TOOLS);
    }
}
