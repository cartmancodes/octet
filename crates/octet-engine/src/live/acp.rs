//! What the Agent Client Protocol providers share: the JSON-RPC frames
//! Octet sends, the replies to permission and stray requests, the reading of
//! a session's options and a tool call, the permission dialog's contents,
//! and the inline image blocks of a prompt.
use super::{
    ApprovalSummary, DriverError, ImageAttachment, ModelInfo, Outcome, SkillInfo,
    activity::one_line, check_inline, limited, protocol::Core, short, valid_identifier,
};
use serde_json::{Value, json};
use std::path::Path;

/// The most models one catalog keeps.
const MAX_MODELS: usize = 256;

/// A JSON-RPC request: `method` with `params`, answered under `id`.
pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    let mut frame = json!({"jsonrpc": "2.0", "id": id, "method": method});
    frame["params"] = params;
    frame
}

/// The `session/cancel` notification for `session`.
pub(super) fn cancel(session: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session},
    })
}

/// The reply to a `session/request_permission`: the request's own
/// `allow_once` or `reject_once` option, never one that allows for longer.
/// `fallback` is the option ID (allow, reject) used when the request lists
/// no such option.
pub(super) fn permission_reply(wire: &Value, allow: bool, fallback: (&str, &str)) -> Value {
    let (kind, fallback) = if allow {
        ("allow_once", fallback.0)
    } else {
        ("reject_once", fallback.1)
    };
    let option = wire
        .pointer("/params/options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option["kind"] == kind)
        .and_then(|option| option["optionId"].as_str())
        .unwrap_or(fallback);
    json!({
        "jsonrpc": "2.0",
        "id": wire["id"],
        "result": {"outcome": {"outcome": "selected", "optionId": option}},
    })
}

/// The reply to a request Octet will not put in front of the user. A
/// permission request is rejected with its `reject_once` option (`fallback`
/// as in [`permission_reply`]); anything else (`fs/*` and `terminal/*`
/// included, which Octet does not offer) is an unsupported method. A frame
/// that is not a request gets none.
pub(super) fn stray_reply(value: &Value, fallback: (&str, &str)) -> Option<Value> {
    let id = value.get("id")?;
    let method = value.get("method")?.as_str()?;
    Some(if method == "session/request_permission" {
        permission_reply(value, false, fallback)
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("Octet does not support {}", short(method))},
        })
    })
}

/// Whether `method` is one of the client capabilities (`fs/*`, `terminal/*`)
/// Octet does not offer in `initialize`: such a request is refused without
/// a notice.
pub(super) fn client_capability(method: &str) -> bool {
    method.starts_with("fs/") || method.starts_with("terminal/")
}

/// An ACP error as a person should read it: its message, else the error
/// itself, bounded.
pub(super) fn error_text(error: &Value) -> String {
    match error["message"].as_str() {
        Some(message) => limited(message),
        None => limited(&error.to_string()),
    }
}

/// The session option `id` in `config_options`, or `Null`.
pub(super) fn option<'a>(config_options: &'a Value, id: &str) -> &'a Value {
    config_options
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["id"] == id)
        .unwrap_or(&Value::Null)
}

/// The current value of the option `id`, when it is one Octet can use.
pub(super) fn current<'a>(config_options: &'a Value, id: &str) -> Option<&'a str> {
    option(config_options, id)["currentValue"]
        .as_str()
        .filter(|value| valid_identifier(value))
}

/// The `model` option's choices as the catalog: each `value` is both what
/// selects the model and its ID, and `name` is the vendor's display name.
/// Values Octet cannot use are dropped, and the list is bounded.
pub(super) fn catalog(config_options: &Value) -> Vec<ModelInfo> {
    option(config_options, "model")["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            let value = choice["value"].as_str().filter(|v| valid_identifier(v))?;
            let name = choice["name"]
                .as_str()
                .filter(|name| !name.is_empty() && name.len() <= 512)
                .unwrap_or(value);
            Some(ModelInfo {
                selection: value.to_owned(),
                id: Some(value.to_owned()),
                name: name.to_owned(),
                description: String::new(),
            })
        })
        .take(MAX_MODELS)
        .collect()
}

/// `availableCommands` as skills: each with a usable name and its
/// description, bounded.
pub(super) fn commands(list: &Value) -> Vec<SkillInfo> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|command| {
            SkillInfo::from_json(
                &json!({"name": command["name"], "description": command["description"]}),
            )
        })
        .take(super::MAX_SKILLS)
        .collect()
}

/// A tool call's `content` as text: the text of each of its blocks.
pub(super) fn content_text(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block.pointer("/content/text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a tool call's frame says the call acts on, in one line: a command's
/// command (its location is only the folder it runs in), else the first path
/// it touches, else its command; `None` when the frame names none of them.
pub(super) fn frame_target(update: &Value) -> Option<String> {
    let command = update.pointer("/rawInput/command").and_then(Value::as_str);
    let target = if update["kind"] == "execute" {
        command
    } else {
        update
            .pointer("/locations/0/path")
            .and_then(Value::as_str)
            .or(command)
    };
    target.map(one_line).filter(|target| !target.is_empty())
}

/// What a permission request's `toolCall` would do: the dialog's detail
/// (`kind · title`, then its `rawInput` in full) and its summary. `known` is
/// the target its tool call named earlier: opencode 1.18.25 asks to read a
/// file without naming it. `vendor` names the CLI where a read names no
/// file.
pub(super) fn approval(
    tool_call: &Value,
    known: Option<&str>,
    vendor: &str,
) -> (String, ApprovalSummary) {
    let kind = tool_call["kind"].as_str().unwrap_or("other");
    let title = tool_call["title"].as_str().unwrap_or("tool");
    let mut detail = format!("{kind} · {title}");
    let input = &tool_call["rawInput"];
    if !input.is_null() {
        detail.push('\n');
        detail.push_str(&serde_json::to_string_pretty(input).unwrap_or_default());
    }
    let locations: Vec<String> = tool_call["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|location| location["path"].as_str())
        .map(str::to_owned)
        .collect();
    let command = input["command"].as_str().map(str::to_owned);
    let summary = match kind {
        "execute" => ApprovalSummary {
            command: Some(title.to_owned()),
            ..ApprovalSummary::new("Run a shell command")
        },
        "edit" => ApprovalSummary {
            files: locations,
            ..ApprovalSummary::new("Edit files")
        },
        "read" => {
            let files = if locations.is_empty() {
                known.map(str::to_owned).into_iter().collect()
            } else {
                locations
            };
            let action = match files.as_slice() {
                [] => format!("Read a file {vendor} did not name"),
                [file] => format!("Read {}", file_name(file)),
                many => format!("Read {} files", many.len()),
            };
            ApprovalSummary {
                files,
                ..ApprovalSummary::new(action)
            }
        }
        "fetch" => {
            let address = input["url"].as_str().unwrap_or(title);
            ApprovalSummary::new(format!("Fetch {}", one_line(address)))
        }
        // A path outside the workspace, for a read or edit, or for a
        // command (opencode's `external_directory` permission is one).
        "other"
            if ["filepath", "parentDir", "directories"]
                .iter()
                .any(|key| !input[key].is_null()) =>
        {
            let path = input["filepath"]
                .as_str()
                .or_else(|| input.pointer("/directories/0").and_then(Value::as_str))
                .or_else(|| input["parentDir"].as_str())
                .or(known)
                .unwrap_or(title);
            ApprovalSummary {
                command,
                files: locations,
                ..ApprovalSummary::new(format!("Reach {}, outside the workspace", one_line(path)))
            }
        }
        _ => ApprovalSummary::new("Use a tool"),
    };
    (detail, summary)
}

/// The last part of `path` in one short line, or the path itself when it
/// has none.
fn file_name(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    one_line(name)
}

/// `images` as inline base64 blocks (`{"type":"image","mimeType","data"}`),
/// read now and checked against what `vendor` accepts inline. An image that
/// cannot be read, or that is too big, fails the turn and gives `Ok(None)`:
/// nothing is to be sent then.
pub(super) async fn image_blocks(
    core: &mut Core,
    vendor: &str,
    images: &[ImageAttachment],
) -> Result<Option<Vec<Value>>, DriverError> {
    let mut encoded = Vec::with_capacity(images.len());
    for image in images {
        // A file swapped for a FIFO, or on a stalled mount, must not hold
        // the session: cancel and stop wait while this runs.
        match tokio::time::timeout(core.limits.image_read, image.read_base64()).await {
            Ok(Ok(data)) => encoded.push((image, data)),
            Ok(Err(error)) => {
                core.finish_turn(Outcome::Failed, Some(error.to_string()))?;
                return Ok(None);
            }
            Err(_) => {
                let message = format!(
                    "Reading {} took longer than {}; the prompt was not sent",
                    image.name,
                    super::driver::seconds(core.limits.image_read)
                );
                core.finish_turn(Outcome::Failed, Some(message))?;
                return Ok(None);
            }
        }
    }
    // Files can change after they were attached; check what is sent.
    let sizes = encoded
        .iter()
        .map(|(image, data)| (image.name.as_str(), data.len() as u64));
    if let Err(error) = check_inline(vendor, sizes) {
        core.finish_turn(Outcome::Failed, Some(error.to_string()))?;
        return Ok(None);
    }
    Ok(Some(
        encoded
            .into_iter()
            .map(|(image, data)| json!({"type": "image", "mimeType": image.media_type, "data": data}))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `configOptions` whose `model` option offers `choices`.
    fn model_options(choices: &Value) -> Value {
        json!([
            {"id": "mode", "currentValue": "build", "options": [{"value": "build", "name": "build"}]},
            {"id": "model", "currentValue": "a/b", "options": choices},
        ])
    }

    #[test]
    fn the_catalog_is_the_model_options_values_and_names() {
        let models = catalog(&model_options(&json!([
            {"value": "opencode/free", "name": "OpenCode/Free"},
            {"value": "bad\u{1b}id", "name": "Bad"},
            {"value": "x".repeat(257), "name": "Long"},
            {"value": "openai/gpt", "name": ""},
            {"name": "No value"},
        ])));
        assert_eq!(
            models,
            [
                ModelInfo {
                    selection: "opencode/free".into(),
                    id: Some("opencode/free".into()),
                    name: "OpenCode/Free".into(),
                    description: String::new(),
                },
                ModelInfo {
                    selection: "openai/gpt".into(),
                    id: Some("openai/gpt".into()),
                    name: "openai/gpt".into(),
                    description: String::new(),
                },
            ]
        );
        let many: Vec<Value> = (0..300)
            .map(|n| json!({"value": format!("p/m{n}"), "name": "M"}))
            .collect();
        assert_eq!(catalog(&model_options(&Value::Array(many))).len(), 256);
        assert!(catalog(&json!([{"id": "mode", "options": []}])).is_empty());
        assert!(catalog(&Value::Null).is_empty());
    }

    #[test]
    fn current_values_are_read_from_their_option() {
        let options = model_options(&json!([]));
        assert_eq!(current(&options, "model"), Some("a/b"));
        assert_eq!(current(&options, "mode"), Some("build"));
        assert_eq!(current(&options, "effort"), None);
    }

    #[test]
    fn an_answer_picks_the_requests_once_option_never_always() {
        let options = json!([
            {"optionId": "ever", "kind": "allow_always"},
            {"optionId": "yes", "kind": "allow_once"},
            {"optionId": "no", "kind": "reject_once"},
        ]);
        let wire = json!({"id": 9, "params": {"options": options}});
        let chosen = |allow| {
            permission_reply(&wire, allow, ("once", "reject"))["result"]["outcome"]["optionId"]
                .clone()
        };
        assert_eq!(chosen(true), "yes");
        assert_eq!(chosen(false), "no");
        // A request without options gets opencode's usual IDs.
        let bare = json!({"id": 9, "params": {}});
        assert_eq!(
            permission_reply(&bare, true, ("once", "reject"))["result"]["outcome"]["optionId"],
            "once"
        );
        assert_eq!(
            permission_reply(&bare, false, ("once", "reject"))["result"]["outcome"]["optionId"],
            "reject"
        );
    }

    #[test]
    fn an_approval_shows_the_tool_call_and_what_it_would_do() {
        let (detail, summary) = approval(
            &json!({
                "kind": "execute", "title": "rm -rf build",
                "rawInput": {"command": "rm -rf build"}, "locations": [],
            }),
            None,
            "opencode",
        );
        assert_eq!(
            detail,
            "execute · rm -rf build\n{\n  \"command\": \"rm -rf build\"\n}"
        );
        assert_eq!(summary.action, "Run a shell command");
        assert_eq!(summary.command.as_deref(), Some("rm -rf build"));
        assert!(summary.files.is_empty());

        let (detail, summary) = approval(
            &json!({
                "kind": "edit", "title": "src/a.rs",
                "locations": [{"path": "/w/src/a.rs"}, {"path": "/w/src/b.rs"}, {"line": 3}],
            }),
            None,
            "opencode",
        );
        assert_eq!(detail, "edit · src/a.rs");
        assert_eq!(summary.action, "Edit files");
        assert_eq!(summary.files, ["/w/src/a.rs", "/w/src/b.rs"]);
        assert_eq!(summary.command, None);

        let (_, summary) = approval(&json!({}), None, "opencode");
        assert_eq!(summary.action, "Use a tool");
        assert_eq!(summary.command, None);
        assert_eq!(approval(&json!({}), None, "opencode").0, "other · tool");
    }

    #[test]
    fn a_read_approval_names_its_file_even_when_the_request_does_not() {
        // opencode 1.18.25 sends a read's request with no path at all; the
        // path is the one its tool call named.
        let request = json!({"kind": "read", "title": "read", "locations": [], "rawInput": {}});
        let (detail, summary) = approval(&request, Some("/w/.env"), "opencode");
        assert_eq!(detail, "read · read\n{}");
        assert_eq!(summary.action, "Read .env");
        assert_eq!(summary.files, ["/w/.env"]);
        let (_, summary) = approval(&request, None, "opencode");
        assert_eq!(summary.action, "Read a file opencode did not name");
        assert!(summary.files.is_empty());
        // A path the request names wins.
        let named = json!({"kind": "read", "title": "a.rs", "locations": [{"path": "/w/a.rs"}]});
        let (_, summary) = approval(&named, Some("/w/.env"), "opencode");
        assert_eq!(summary.action, "Read a.rs");
        assert_eq!(summary.files, ["/w/a.rs"]);
    }

    #[test]
    fn a_fetch_approval_names_its_address() {
        let request = json!({"kind": "fetch", "title": "https://example.com", "locations": [],
            "rawInput": {"url": "https://example.com/a", "format": "text"}});
        let (_, summary) = approval(&request, None, "opencode");
        assert_eq!(summary.action, "Fetch https://example.com/a");
        assert!(summary.files.is_empty());
        let (_, summary) = approval(
            &json!({"kind": "fetch", "title": "https://b.example"}),
            None,
            "opencode",
        );
        assert_eq!(summary.action, "Fetch https://b.example");
    }

    #[test]
    fn an_outside_the_workspace_approval_names_the_path_and_any_command() {
        // A read outside the workspace, as opencode 1.18.25 asks for it.
        let read = json!({"kind": "other", "title": "/etc",
            "locations": [{"path": "/etc/hosts"}, {"path": "/etc"}],
            "rawInput": {"filepath": "/etc/hosts", "parentDir": "/etc"}});
        let (_, summary) = approval(&read, Some("/etc/hosts"), "opencode");
        assert_eq!(summary.action, "Reach /etc/hosts, outside the workspace");
        assert_eq!(summary.files, ["/etc/hosts", "/etc"]);
        assert_eq!(summary.command, None);
        // A command that touches another folder.
        let command = json!({"kind": "other", "title": "touch /h/x.txt",
            "locations": [{"path": "/h"}],
            "rawInput": {"command": "touch /h/x.txt", "directories": ["/h"], "patterns": ["/h/*"]}});
        let (_, summary) = approval(&command, None, "opencode");
        assert_eq!(summary.action, "Reach /h, outside the workspace");
        assert_eq!(summary.command.as_deref(), Some("touch /h/x.txt"));
        assert_eq!(summary.files, ["/h"]);
    }
}
