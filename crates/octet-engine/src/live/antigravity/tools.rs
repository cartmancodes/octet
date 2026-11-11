//! The running turn's tool calls, as Antigravity's frames name them: each
//! call's name, target and kind, and whether a permission request named it.
use crate::live::{ToolCall, acp, activity::one_line};
use serde_json::Value;
use std::collections::HashMap;

/// The most tool calls one turn remembers.
pub(super) const MAX_TOOLS: usize = 1024;

/// A tool call of the running turn, as its first frame named it.
struct Tool {
    /// The tool's name, else its title.
    name: String,
    /// The newest target a frame named.
    target: String,
    /// The kind of its first frame; later frames need not repeat it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the tripwire, which a later commit adds")
    )]
    kind: String,
    /// A permission request named the call this turn: a call that ends
    /// without one ran unasked. The tripwire, which a later commit adds,
    /// reads it.
    asked: bool,
}

/// This turn's tool calls, by ID, at most [`MAX_TOOLS`].
#[derive(Default)]
pub(super) struct Tools {
    /// The calls, by ID.
    calls: HashMap<String, Tool>,
}

impl Tools {
    /// Forgets every call: a turn starts or ends.
    pub(super) fn clear(&mut self) {
        self.calls.clear();
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
    /// the request names it when it is not there yet. A call without an ID
    /// cannot be named again, and a full table takes no more.
    pub(super) fn mark_asked(&mut self, tool_call: &Value) {
        let id = one_line(tool_call["toolCallId"].as_str().unwrap_or_default());
        if id.is_empty() {
            return;
        }
        if let Some(tool) = self.calls.get_mut(&id) {
            tool.asked = true;
        } else if self.calls.len() < MAX_TOOLS {
            let mut tool = Tool::first(tool_call, acp::frame_target(tool_call));
            tool.asked = true;
            self.calls.insert(id, tool);
        }
    }

    /// A `tool_call` or `tool_call_update`: the call as it now stands. Its
    /// name and kind are its first frame's; its target is the newest one a
    /// frame names (see `acp::frame_target`), else a command's title. At most
    /// [`MAX_TOOLS`] calls are remembered in a turn.
    pub(super) fn frame(&mut self, update: &Value) -> ToolCall {
        let id = one_line(update["toolCallId"].as_str().unwrap_or_default());
        let target = acp::frame_target(update);
        let (name, target) = if let Some(known) = self.calls.get_mut(&id) {
            if let Some(target) = target {
                known.target = target;
            }
            (known.name.clone(), known.target.clone())
        } else {
            let tool = Tool::first(update, target);
            let shown = (tool.name.clone(), tool.target.clone());
            if !id.is_empty() && self.calls.len() < MAX_TOOLS {
                self.calls.insert(id.clone(), tool);
            }
            shown
        };
        ToolCall {
            id,
            name,
            target,
            phase: acp::tool_phase(&update["status"]),
            detail: acp::content_text(&update["content"]),
        }
    }
}

impl Tool {
    /// A call as its first frame `update` names it, with the `target` that
    /// frame names, if any.
    fn first(update: &Value, target: Option<String>) -> Self {
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
            name: named("name").or(title).unwrap_or_else(|| "tool".to_owned()),
            target,
            kind,
            asked: false,
        }
    }
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
        );
        assert_eq!(
            (tool.name.as_str(), tool.target.as_str()),
            ("run_command", "ls -la")
        );
        assert_eq!(tool.kind, "execute");
        let tool = Tool::first(&json!({"kind": "read", "title": "Read file"}), None);
        assert_eq!(
            (tool.name.as_str(), tool.target.as_str()),
            ("Read file", "")
        );
        let tool = Tool::first(&json!({}), Some("/w/a".into()));
        assert_eq!((tool.name.as_str(), tool.target.as_str()), ("tool", "/w/a"));
        assert_eq!(tool.kind, "other");
    }

    #[test]
    fn a_permission_request_marks_its_tool_call_asked() {
        let mut tools = Tools::default();
        let call = |id: &str| json!({"toolCallId": id, "name": "run_command", "kind": "execute", "title": "ls"});
        tools
            .calls
            .insert("known".to_owned(), Tool::first(&call("known"), None));
        tools.mark_asked(&call("known"));
        tools.mark_asked(&call("new"));
        // A call with no ID cannot be named again.
        tools.mark_asked(&json!({"kind": "execute"}));
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
                .insert(n.to_string(), Tool::first(&call("x"), None));
        }
        tools.mark_asked(&call("late"));
        assert!(!tools.calls.contains_key("late"));
        assert_eq!(tools.calls.len(), MAX_TOOLS);
    }
}
