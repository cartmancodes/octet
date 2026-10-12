//! The remote-control wire protocol, defined once: every message a terminal UI, the hub and a
//! phone exchange is a row in [`MESSAGES`], and the Rust parsers, the frame builders and the
//! app's TypeScript (`apps/mobile/src/protocol.ts`) all follow that table.
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

/// The protocol version every message carries as `v`.
pub const VERSION: u64 = 1;
/// The most bytes one message may take (a 64 KiB prompt, JSON-escaped, with room).
pub const MAX_MESSAGE: usize = 96 * 1024;
/// What a client must type to confirm `full-access`.
pub const FULL_ACCESS_CONFIRM: &str = "full access";

/// Which link a message travels on, and in which direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Hub to phone, over the WebSocket.
    ToPhone,
    /// Phone to hub, over the WebSocket.
    FromPhone,
    /// Hub to a terminal UI's bridge, over its Unix socket.
    ToBridge,
    /// A terminal UI's bridge to the hub.
    FromBridge,
}

/// The type of one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// A string.
    Str,
    /// A non-negative whole number.
    U64,
    /// A boolean.
    Bool,
    /// Any JSON value, passed through unchecked.
    Json,
    /// A list of strings.
    StrList,
    /// An object of the named shape (a row of [`OBJECTS`]).
    Object(&'static str),
    /// A list of objects of the named shape.
    ObjectList(&'static str),
}

/// One field of a message: its name, its type and whether it can be left out.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// The JSON key.
    pub name: &'static str,
    /// What the value must be.
    pub ty: Ty,
    /// Whether the key can be absent (or null).
    pub optional: bool,
}

/// One message type.
#[derive(Debug, Clone, Copy)]
pub struct Message {
    /// The `type` value.
    pub kind: &'static str,
    /// The link and direction it travels.
    pub direction: Direction,
    /// Its fields besides `v`, `type` and (for commands) `req`.
    pub fields: &'static [Field],
    /// One line saying what it is for.
    pub doc: &'static str,
}

/// One named object shape that frames embed.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    /// The shape's name (its TypeScript interface).
    pub name: &'static str,
    /// Its fields.
    pub fields: &'static [Field],
    /// One line saying what it is for.
    pub doc: &'static str,
}

const fn req(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        optional: false,
    }
}
const fn opt(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        optional: true,
    }
}
const fn message(
    kind: &'static str,
    direction: Direction,
    fields: &'static [Field],
    doc: &'static str,
) -> Message {
    Message {
        kind,
        direction,
        fields,
        doc,
    }
}

use Direction::{FromBridge, FromPhone, ToBridge, ToPhone};
use Ty::{Bool, Json, Object, ObjectList, Str, StrList, U64};

/// The object shapes frames embed, in the order the TypeScript declares them.
pub static OBJECTS: &[Shape] = &[
    Shape {
        name: "Cursor",
        fields: &[req("journal", Str), req("seq", U64)],
        doc: "A place in a session's history: a journal file name and the record's sequence number.",
    },
    Shape {
        name: "SessionInfo",
        fields: &[
            req("id", Str),
            req("engine", Str),
            opt("model", Str),
            req("workspace", Str),
            req("mode", Str),
            req("phase", Str),
            req("approvals", U64),
            opt("last_activity", U64),
            opt("goal", Str),
            req("from_phone", Bool),
        ],
        doc: "One live session; `last_activity` is Unix seconds, `goal` a one-liner.",
    },
    Shape {
        name: "GoalInfo",
        fields: &[
            req("objective", Str),
            req("status", Str),
            req("turns", U64),
            req("max", U64),
        ],
        doc: "The goal a terminal UI is pursuing.",
    },
    Shape {
        name: "ApprovalInfo",
        fields: &[req("id", U64), req("summary", Json)],
        doc: "An approval waiting for an answer, with its summary as the journal records it.",
    },
    Shape {
        name: "StateInfo",
        fields: &[
            req("phase", Str),
            req("queue", StrList),
            req("queued", U64),
            opt("goal", Object("GoalInfo")),
            opt("now", Str),
            req("mode", Str),
            req("approvals", ObjectList("ApprovalInfo")),
        ],
        doc: "What only the terminal UI knows: queue, goal, NOW line, mode and pending approvals.",
    },
    Shape {
        name: "ModelInfo",
        fields: &[
            req("engine", Str),
            req("selection", Str),
            req("name", Str),
            req("id", Str),
        ],
        doc: "One model a session can switch to.",
    },
    Shape {
        name: "ModelRef",
        fields: &[req("engine", Str), req("selection", Str)],
        doc: "The model a session is using now.",
    },
    Shape {
        name: "CatalogInfo",
        fields: &[
            req("modes", StrList),
            req("current_mode", Str),
            req("models", ObjectList("ModelInfo")),
            opt("current", Object("ModelRef")),
        ],
        doc: "The modes, and every provider's models, with the current ones marked.",
    },
    Shape {
        name: "RecentInfo",
        fields: &[
            req("engine", Str),
            req("workspace", Str),
            req("session", Str),
            req("title", Str),
            opt("model", Str),
            req("started", U64),
        ],
        doc: "One resumable session; `started` is Unix seconds.",
    },
];

/// Every message type on every link.
pub static MESSAGES: &[Message] = &[
    message(
        "welcome",
        ToPhone,
        &[req("device", Str)],
        "A known device connected.",
    ),
    message(
        "paired",
        ToPhone,
        &[req("token", Str), req("device", Str)],
        "A pairing succeeded; keep the token.",
    ),
    message(
        "sessions",
        ToPhone,
        &[req("sessions", ObjectList("SessionInfo"))],
        "The live sessions.",
    ),
    message(
        "event",
        ToPhone,
        &[
            req("session", Str),
            req("journal", Str),
            req("seq", U64),
            req("event", Json),
        ],
        "One journal record of a session, live or replayed.",
    ),
    message(
        "state",
        ToPhone,
        &[req("session", Str), req("state", Object("StateInfo"))],
        "A session's terminal UI state.",
    ),
    message(
        "catalog",
        ToPhone,
        &[req("session", Str), req("catalog", Object("CatalogInfo"))],
        "A session's modes and models.",
    ),
    message(
        "recent",
        ToPhone,
        &[req("items", ObjectList("RecentInfo"))],
        "Recent sessions that can be resumed.",
    ),
    message(
        "ack",
        ToPhone,
        &[req("req", U64), req("message", Str)],
        "A command succeeded.",
    ),
    message(
        "refused",
        ToPhone,
        &[req("req", U64), req("reason", Str)],
        "A command was refused, with the reason.",
    ),
    message(
        "hello",
        FromPhone,
        &[req("token", Str)],
        "Connect with a device token.",
    ),
    message(
        "pair",
        FromPhone,
        &[req("secret", Str), req("name", Str), req("platform", Str)],
        "Pair a new device with a pairing secret.",
    ),
    message(
        "subscribe",
        FromPhone,
        &[req("session", Str), opt("after", Object("Cursor"))],
        "Replay a session after a cursor, then stream it live.",
    ),
    message(
        "unsubscribe",
        FromPhone,
        &[req("session", Str)],
        "Stop streaming a session.",
    ),
    message(
        "prompt",
        FromPhone,
        &[req("session", Str), req("text", Str)],
        "Send a prompt, or queue it while a turn runs.",
    ),
    message(
        "answer",
        FromPhone,
        &[
            req("session", Str),
            req("approval", U64),
            req("allow", Bool),
        ],
        "Allow once, or deny, an approval.",
    ),
    message(
        "cancel",
        FromPhone,
        &[req("session", Str)],
        "Cancel the running turn.",
    ),
    message(
        "steer",
        FromPhone,
        &[req("session", Str), req("text", Str)],
        "Steer the running turn (Codex only).",
    ),
    message(
        "set_mode",
        FromPhone,
        &[req("session", Str), req("mode", Str), opt("confirm", Str)],
        "Change the permission mode; full-access needs `confirm`.",
    ),
    message(
        "set_model",
        FromPhone,
        &[req("session", Str), req("selection", Str)],
        "Switch the model.",
    ),
    message(
        "recent",
        FromPhone,
        &[opt("workspace", Str)],
        "List recent sessions, optionally of one workspace.",
    ),
    message(
        "start",
        FromPhone,
        &[
            req("engine", Str),
            req("workspace", Str),
            opt("resume", Str),
            opt("model", Str),
            opt("mode", Str),
            opt("confirm", Str),
        ],
        "Start or resume a terminal UI in tmux.",
    ),
    message(
        "end",
        FromPhone,
        &[req("session", Str)],
        "Quit a terminal UI; refused while a turn runs.",
    ),
    message(
        "prompt",
        ToBridge,
        &[req("by", Str), req("text", Str)],
        "Send a prompt, or queue it.",
    ),
    message(
        "answer",
        ToBridge,
        &[req("by", Str), req("approval", U64), req("allow", Bool)],
        "Answer an approval.",
    ),
    message(
        "cancel",
        ToBridge,
        &[req("by", Str)],
        "Cancel the running turn.",
    ),
    message(
        "steer",
        ToBridge,
        &[req("by", Str), req("text", Str)],
        "Steer the running turn.",
    ),
    message(
        "set_mode",
        ToBridge,
        &[req("by", Str), req("mode", Str), opt("confirm", Str)],
        "Change the mode.",
    ),
    message(
        "set_model",
        ToBridge,
        &[req("by", Str), req("selection", Str)],
        "Switch the model.",
    ),
    message("end", ToBridge, &[req("by", Str)], "Quit the terminal UI."),
    message(
        "state",
        FromBridge,
        &[req("state", Object("StateInfo"))],
        "The terminal UI's state changed.",
    ),
    message(
        "catalog",
        FromBridge,
        &[req("catalog", Object("CatalogInfo"))],
        "The modes and models.",
    ),
    message(
        "ack",
        FromBridge,
        &[req("req", U64), req("message", Str)],
        "A command succeeded.",
    ),
    message(
        "refused",
        FromBridge,
        &[req("req", U64), req("reason", Str)],
        "A command was refused, with the reason.",
    ),
];

/// A place in a session's history, as a phone resumes a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The journal file name.
    pub journal: String,
    /// The record's sequence number within it.
    pub seq: u64,
}

/// A phone → hub command, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneCommand {
    /// Connect with a device token.
    Hello {
        /// The device token.
        token: String,
    },
    /// Pair a new device.
    Pair {
        /// The single-use pairing secret.
        secret: String,
        /// The device's name.
        name: String,
        /// `ios` or `android`.
        platform: String,
    },
    /// Replay a session after `after`, then stream it.
    Subscribe {
        /// The session id.
        session: String,
        /// The last record the phone has.
        after: Option<Cursor>,
    },
    /// Stop streaming a session.
    Unsubscribe {
        /// The session id.
        session: String,
    },
    /// Send a prompt.
    Prompt {
        /// The session id.
        session: String,
        /// The prompt.
        text: String,
    },
    /// Answer an approval.
    Answer {
        /// The session id.
        session: String,
        /// The approval's id.
        approval: u64,
        /// Allow once, or deny.
        allow: bool,
    },
    /// Cancel the running turn.
    Cancel {
        /// The session id.
        session: String,
    },
    /// Steer the running turn.
    Steer {
        /// The session id.
        session: String,
        /// The steering text.
        text: String,
    },
    /// Change the permission mode.
    SetMode {
        /// The session id.
        session: String,
        /// The mode's label.
        mode: String,
        /// The confirmation `full-access` needs.
        confirm: Option<String>,
    },
    /// Switch the model.
    SetModel {
        /// The session id.
        session: String,
        /// The model selection.
        selection: String,
    },
    /// List recent sessions.
    Recent {
        /// Only this workspace's.
        workspace: Option<String>,
    },
    /// Start a terminal UI in tmux.
    Start {
        /// The provider.
        engine: String,
        /// The workspace directory.
        workspace: String,
        /// A vendor session to resume.
        resume: Option<String>,
        /// A model to start with.
        model: Option<String>,
        /// A mode to start in.
        mode: Option<String>,
        /// The confirmation `full-access` needs.
        confirm: Option<String>,
    },
    /// Quit a terminal UI.
    End {
        /// The session id.
        session: String,
    },
}

/// A hub → bridge command (the subset a terminal UI runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeCommand {
    /// Send a prompt, or queue it.
    Prompt {
        /// The prompt.
        text: String,
    },
    /// Answer an approval.
    Answer {
        /// The approval's id.
        approval: u64,
        /// Allow once, or deny.
        allow: bool,
    },
    /// Cancel the running turn.
    Cancel,
    /// Steer the running turn.
    Steer {
        /// The steering text.
        text: String,
    },
    /// Change the permission mode.
    SetMode {
        /// The mode's label.
        mode: String,
        /// The confirmation `full-access` needs.
        confirm: Option<String>,
    },
    /// Switch the model.
    SetModel {
        /// The model selection.
        selection: String,
    },
    /// Quit the terminal UI.
    End,
}

/// The keys every command carries beside its `fields`.
const COMMON: &[&str] = &["v", "req", "type"];

fn type_name(ty: Ty) -> &'static str {
    match ty {
        Ty::Str => "string",
        Ty::U64 => "non-negative number",
        Ty::Bool => "boolean",
        Ty::Json => "JSON value",
        Ty::StrList => "list of strings",
        Ty::Object(_) => "object",
        Ty::ObjectList(_) => "list of objects",
    }
}

fn shape(name: &str) -> &'static [Field] {
    OBJECTS
        .iter()
        .find(|s| s.name == name)
        .map_or(&[], |s| s.fields)
}

/// Checks `value` against `ty`; `name` is the dotted path for messages.
fn check_value(value: &Value, ty: Ty, name: &str) -> Result<(), String> {
    let wrong = || format!("{name} must be a {}", type_name(ty));
    match ty {
        Ty::Json => Ok(()),
        Ty::Str if value.is_string() => Ok(()),
        Ty::U64 if value.is_u64() => Ok(()),
        Ty::Bool if value.is_boolean() => Ok(()),
        Ty::StrList
            if value
                .as_array()
                .is_some_and(|l| l.iter().all(Value::is_string)) =>
        {
            Ok(())
        }
        Ty::Object(shape_name) => {
            let map = value.as_object().ok_or_else(wrong)?;
            check_fields(map, shape(shape_name), &[], name)
        }
        Ty::ObjectList(shape_name) => {
            let list = value.as_array().ok_or_else(wrong)?;
            list.iter().enumerate().try_for_each(|(n, item)| {
                let map = item.as_object().ok_or_else(wrong)?;
                check_fields(map, shape(shape_name), &[], &format!("{name}[{n}]"))
            })
        }
        _ => Err(wrong()),
    }
}

/// Checks an object against `fields`; keys in `common` are allowed besides them. `path` prefixes
/// the names of nested fields.
fn check_fields(
    map: &Map<String, Value>,
    fields: &[Field],
    common: &[&str],
    path: &str,
) -> Result<(), String> {
    let at = |name: &str| {
        if path.is_empty() {
            name.to_owned()
        } else {
            format!("{path}.{name}")
        }
    };
    for field in fields {
        match map.get(field.name) {
            None | Some(Value::Null) if field.optional => {}
            None | Some(Value::Null) => return Err(format!("Missing {}", at(field.name))),
            Some(value) => check_value(value, field.ty, &at(field.name))?,
        }
    }
    match map
        .keys()
        .find(|k| !common.contains(&k.as_str()) && !fields.iter().any(|f| f.name == k.as_str()))
    {
        Some(key) => Err(format!("Unexpected field {}", at(key))),
        None => Ok(()),
    }
}

/// Parses and checks one inbound message of `direction`: its `req`, its definition and its object.
fn parse_message(
    text: &str,
    direction: Direction,
) -> Result<(u64, &'static Message, Map<String, Value>), String> {
    if text.len() > MAX_MESSAGE {
        return Err(format!(
            "Message too large; the limit is {MAX_MESSAGE} bytes"
        ));
    }
    let value: Value = serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}"))?;
    let Value::Object(map) = value else {
        return Err("Message must be a JSON object".into());
    };
    if map.get("v").and_then(Value::as_u64) != Some(VERSION) {
        return Err(format!(
            "Unsupported protocol version; this hub speaks version {VERSION}"
        ));
    }
    let req = match map.get("req") {
        None => return Err("Missing req".into()),
        Some(value) => value.as_u64().ok_or("req must be a non-negative number")?,
    };
    let kind = match map.get("type") {
        None => return Err("Missing type".into()),
        Some(value) => value.as_str().ok_or("type must be a string")?,
    };
    let message = MESSAGES
        .iter()
        .find(|m| m.direction == direction && m.kind == kind)
        .ok_or_else(|| format!("Unknown message {kind}"))?;
    check_fields(&map, message.fields, COMMON, "")?;
    Ok((req, message, map))
}

fn text(map: &Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
fn maybe(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn number(map: &Map<String, Value>, key: &str) -> u64 {
    map.get(key).and_then(Value::as_u64).unwrap_or_default()
}
fn flag(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or_default()
}

/// Parses one phone message: `(req, command)`, or the reason it was refused.
///
/// # Errors
///
/// The reason, in words for the phone: a message too large, not JSON, of another version, of an
/// unknown type, or with a field missing, mistyped or unexpected.
pub fn parse_phone(text_in: &str) -> Result<(u64, PhoneCommand), String> {
    let (req, message, map) = parse_message(text_in, FromPhone)?;
    let m = &map;
    let command = match message.kind {
        "hello" => PhoneCommand::Hello {
            token: text(m, "token"),
        },
        "pair" => PhoneCommand::Pair {
            secret: text(m, "secret"),
            name: text(m, "name"),
            platform: text(m, "platform"),
        },
        "subscribe" => PhoneCommand::Subscribe {
            session: text(m, "session"),
            after: m.get("after").and_then(Value::as_object).map(|c| Cursor {
                journal: text(c, "journal"),
                seq: number(c, "seq"),
            }),
        },
        "unsubscribe" => PhoneCommand::Unsubscribe {
            session: text(m, "session"),
        },
        "prompt" => PhoneCommand::Prompt {
            session: text(m, "session"),
            text: text(m, "text"),
        },
        "answer" => PhoneCommand::Answer {
            session: text(m, "session"),
            approval: number(m, "approval"),
            allow: flag(m, "allow"),
        },
        "cancel" => PhoneCommand::Cancel {
            session: text(m, "session"),
        },
        "steer" => PhoneCommand::Steer {
            session: text(m, "session"),
            text: text(m, "text"),
        },
        "set_mode" => PhoneCommand::SetMode {
            session: text(m, "session"),
            mode: text(m, "mode"),
            confirm: maybe(m, "confirm"),
        },
        "set_model" => PhoneCommand::SetModel {
            session: text(m, "session"),
            selection: text(m, "selection"),
        },
        "recent" => PhoneCommand::Recent {
            workspace: maybe(m, "workspace"),
        },
        "start" => PhoneCommand::Start {
            engine: text(m, "engine"),
            workspace: text(m, "workspace"),
            resume: maybe(m, "resume"),
            model: maybe(m, "model"),
            mode: maybe(m, "mode"),
            confirm: maybe(m, "confirm"),
        },
        "end" => PhoneCommand::End {
            session: text(m, "session"),
        },
        other => return Err(format!("Unknown message {other}")),
    };
    Ok((req, command))
}

/// A hub → bridge line for `command`, sent on behalf of the device `by`.
pub fn bridge_command_json(req: u64, by: &str, command: &BridgeCommand) -> Value {
    let (kind, fields) = match command {
        BridgeCommand::Prompt { text } => ("prompt", json!({ "text": text })),
        BridgeCommand::Answer { approval, allow } => {
            ("answer", json!({ "approval": approval, "allow": allow }))
        }
        BridgeCommand::Cancel => ("cancel", json!({})),
        BridgeCommand::Steer { text } => ("steer", json!({ "text": text })),
        BridgeCommand::SetMode { mode, confirm } => {
            let mut fields = json!({ "mode": mode });
            if let Some(confirm) = confirm {
                fields["confirm"] = json!(confirm);
            }
            ("set_mode", fields)
        }
        BridgeCommand::SetModel { selection } => ("set_model", json!({ "selection": selection })),
        BridgeCommand::End => ("end", json!({})),
    };
    let mut line = json!({ "v": VERSION, "req": req, "type": kind, "by": by });
    if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    line
}

/// Parses one hub → bridge line: `(req, device, command)`, or the reason it was refused.
///
/// # Errors
///
/// The reason: a line too large, not JSON, of another version, of an unknown type, or with a
/// field missing, mistyped or unexpected.
pub fn parse_bridge_command(line: &str) -> Result<(u64, String, BridgeCommand), String> {
    let (req, message, map) = parse_message(line, ToBridge)?;
    let m = &map;
    let command = match message.kind {
        "prompt" => BridgeCommand::Prompt {
            text: text(m, "text"),
        },
        "answer" => BridgeCommand::Answer {
            approval: number(m, "approval"),
            allow: flag(m, "allow"),
        },
        "cancel" => BridgeCommand::Cancel,
        "steer" => BridgeCommand::Steer {
            text: text(m, "text"),
        },
        "set_mode" => BridgeCommand::SetMode {
            mode: text(m, "mode"),
            confirm: maybe(m, "confirm"),
        },
        "set_model" => BridgeCommand::SetModel {
            selection: text(m, "selection"),
        },
        "end" => BridgeCommand::End,
        other => return Err(format!("Unknown message {other}")),
    };
    Ok((req, text(m, "by"), command))
}

fn frame(kind: &str, fields: Value) -> Value {
    let mut frame = json!({ "v": VERSION, "type": kind });
    if let (Some(frame), Value::Object(fields)) = (frame.as_object_mut(), fields) {
        frame.extend(fields);
    }
    frame
}

/// The `welcome` frame for a known device.
pub fn welcome(device: &str) -> Value {
    frame("welcome", json!({ "device": device }))
}
/// The `paired` frame carrying the new device token.
pub fn paired(token: &str, device: &str) -> Value {
    frame("paired", json!({ "token": token, "device": device }))
}
/// The `sessions` frame: each row is a `SessionInfo`.
pub fn sessions_frame(sessions: &[Value]) -> Value {
    frame("sessions", json!({ "sessions": sessions }))
}
/// The `event` frame for one journal record (`event` is the record, as the journal wrote it).
pub fn event_frame(session: &str, journal: &str, seq: u64, event: &Value) -> Value {
    frame(
        "event",
        json!({ "session": session, "journal": journal, "seq": seq, "event": event }),
    )
}
/// The `state` frame: a session's `StateInfo`.
pub fn state_frame(session: &str, state: &Value) -> Value {
    frame("state", json!({ "session": session, "state": state }))
}
/// The `catalog` frame: a session's `CatalogInfo`.
pub fn catalog_frame(session: &str, catalog: &Value) -> Value {
    frame("catalog", json!({ "session": session, "catalog": catalog }))
}
/// The `recent` frame: each row is a `RecentInfo`.
pub fn recent_frame(items: &[Value]) -> Value {
    frame("recent", json!({ "items": items }))
}
/// The `ack` frame (hub → phone, or bridge → hub) for the command `req`.
pub fn ack(req: u64, message: &str) -> Value {
    frame("ack", json!({ "req": req, "message": message }))
}
/// The `refused` frame (hub → phone, or bridge → hub) for the command `req`, with the reason.
pub fn refused(req: u64, reason: &str) -> Value {
    frame("refused", json!({ "req": req, "reason": reason }))
}
/// The bridge → hub `state` frame: the terminal UI's `StateInfo`.
pub fn bridge_state_frame(state: &Value) -> Value {
    frame("state", json!({ "state": state }))
}
/// The bridge → hub `catalog` frame: the terminal UI's `CatalogInfo`.
pub fn bridge_catalog_frame(catalog: &Value) -> Value {
    frame("catalog", json!({ "catalog": catalog }))
}

fn pascal(kind: &str) -> String {
    kind.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect()
}

/// The TypeScript interface name of a message: its direction keeps same-named kinds apart.
fn interface_name(message: &Message) -> String {
    let kind = pascal(message.kind);
    match message.direction {
        ToPhone => format!("{kind}Frame"),
        FromPhone => format!("{kind}Command"),
        ToBridge => format!("Bridge{kind}Command"),
        FromBridge => format!("Bridge{kind}Frame"),
    }
}

fn ts_type(ty: Ty) -> String {
    match ty {
        Ty::Str => "string".into(),
        Ty::U64 => "number".into(),
        Ty::Bool => "boolean".into(),
        Ty::Json => "unknown".into(),
        Ty::StrList => "string[]".into(),
        Ty::Object(name) => name.into(),
        Ty::ObjectList(name) => format!("{name}[]"),
    }
}

fn ts_fields(out: &mut String, fields: &[Field]) {
    for field in fields {
        let mark = if field.optional { "?" } else { "" };
        let _ = writeln!(out, "  {}{mark}: {};", field.name, ts_type(field.ty));
    }
}

/// The checked-in TypeScript for the app, generated from [`MESSAGES`] and [`OBJECTS`].
pub fn typescript() -> String {
    let mut out = String::from("// Generated by octet-core (remote::protocol). Do not edit.\n");
    let _ = writeln!(out, "export const PROTOCOL_VERSION = {VERSION};");
    for object in OBJECTS {
        let _ = writeln!(
            out,
            "\n/** {} */\nexport interface {} {{",
            object.doc, object.name
        );
        ts_fields(&mut out, object.fields);
        out.push_str("}\n");
    }
    for message in MESSAGES {
        let _ = writeln!(
            out,
            "\n/** {} */\nexport interface {} {{",
            message.doc,
            interface_name(message)
        );
        let _ = writeln!(out, "  v: {VERSION};\n  type: \"{}\";", message.kind);
        if matches!(message.direction, FromPhone | ToBridge) {
            out.push_str("  req: number;\n");
        }
        ts_fields(&mut out, message.fields);
        out.push_str("}\n");
    }
    for (name, direction) in [
        ("ToPhone", ToPhone),
        ("FromPhone", FromPhone),
        ("ToBridge", ToBridge),
        ("FromBridge", FromBridge),
    ] {
        let members: Vec<String> = MESSAGES
            .iter()
            .filter(|m| m.direction == direction)
            .map(interface_name)
            .collect();
        let _ = writeln!(
            out,
            "\nexport type {name} =\n  | {};",
            members.join("\n  | ")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn every_phone_command_parses() {
        let cases = [
            r#"{"v":1,"req":1,"type":"hello","token":"t"}"#,
            r#"{"v":1,"req":2,"type":"pair","secret":"s","name":"Gemini 10","platform":"android"}"#,
            r#"{"v":1,"req":3,"type":"subscribe","session":"12-34","after":{"journal":"session-1.jsonl","seq":7}}"#,
            r#"{"v":1,"req":4,"type":"subscribe","session":"12-34"}"#,
            r#"{"v":1,"req":5,"type":"prompt","session":"12-34","text":"hi"}"#,
            r#"{"v":1,"req":6,"type":"answer","session":"12-34","approval":3,"allow":true}"#,
            r#"{"v":1,"req":7,"type":"cancel","session":"12-34"}"#,
            r#"{"v":1,"req":8,"type":"steer","session":"12-34","text":"also"}"#,
            r#"{"v":1,"req":9,"type":"set_mode","session":"12-34","mode":"full-access","confirm":"full access"}"#,
            r#"{"v":1,"req":10,"type":"set_model","session":"12-34","selection":"gpt-5.5"}"#,
            r#"{"v":1,"req":11,"type":"recent"}"#,
            r#"{"v":1,"req":12,"type":"start","engine":"claude","workspace":"/w","resume":"abc"}"#,
            r#"{"v":1,"req":13,"type":"end","session":"12-34"}"#,
            r#"{"v":1,"req":14,"type":"unsubscribe","session":"12-34"}"#,
        ];
        for (n, case) in cases.iter().enumerate() {
            let (req, _) = parse_phone(case).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(req, n as u64 + 1);
        }
        assert!(matches!(
            parse_phone(cases[2]).unwrap().1,
            PhoneCommand::Subscribe {
                after: Some(Cursor { seq: 7, .. }),
                ..
            }
        ));
    }

    #[test]
    fn parsed_commands_carry_their_fields() {
        let (_, start) = parse_phone(
            r#"{"v":1,"req":1,"type":"start","engine":"claude","workspace":"/w","resume":"abc","model":null}"#,
        )
        .unwrap();
        assert_eq!(
            start,
            PhoneCommand::Start {
                engine: "claude".into(),
                workspace: "/w".into(),
                resume: Some("abc".into()),
                model: None,
                mode: None,
                confirm: None,
            }
        );
        let (_, answer) = parse_phone(
            r#"{"v":1,"req":1,"type":"answer","session":"s","approval":3,"allow":false}"#,
        )
        .unwrap();
        assert_eq!(
            answer,
            PhoneCommand::Answer {
                session: "s".into(),
                approval: 3,
                allow: false
            }
        );
        let (_, subscribe) =
            parse_phone(r#"{"v":1,"req":1,"type":"subscribe","session":"s","after":null}"#)
                .unwrap();
        assert_eq!(
            subscribe,
            PhoneCommand::Subscribe {
                session: "s".into(),
                after: None
            }
        );
    }

    #[test]
    fn malformed_messages_say_why() {
        for (text, wanted) in [
            ("not json", "JSON"),
            (r#"{"v":2,"req":1,"type":"hello","token":"t"}"#, "version 1"),
            (
                r#"{"v":1,"req":1,"type":"launch"}"#,
                "Unknown message launch",
            ),
            (r#"{"v":1,"type":"hello","token":"t"}"#, "req"),
            (r#"{"v":1,"req":1,"type":"prompt","session":"s"}"#, "text"),
            (
                r#"{"v":1,"req":1,"type":"answer","session":"s","approval":"x","allow":true}"#,
                "approval",
            ),
            (
                r#"{"v":1,"req":1,"type":"hello","token":"t","extra":1}"#,
                "extra",
            ),
            (
                r#"{"v":1,"req":1,"type":"subscribe","session":"s","after":{"journal":"j"}}"#,
                "seq",
            ),
            (
                r#"{"v":1,"req":1,"type":"subscribe","session":"s","after":{"journal":"j","seq":1,"x":2}}"#,
                "x",
            ),
            ("[1]", "object"),
        ] {
            let error = parse_phone(text).unwrap_err();
            assert!(error.contains(wanted), "{text}: {error}");
        }
        assert!(
            parse_phone(&format!(
                r#"{{"v":1,"req":1,"type":"prompt","session":"s","text":"{}"}}"#,
                "x".repeat(MAX_MESSAGE)
            ))
            .unwrap_err()
            .contains("too large")
        );
    }

    #[test]
    fn a_hub_frame_is_not_a_phone_command() {
        let error = parse_phone(&ack(1, "ok").to_string()).unwrap_err();
        assert!(error.contains("Unknown message ack"), "{error}");
    }

    #[test]
    fn bridge_commands_round_trip() {
        let command = BridgeCommand::SetMode {
            mode: "auto".into(),
            confirm: None,
        };
        let line = bridge_command_json(9, "Gemini 10", &command).to_string();
        let (req, by, parsed) = parse_bridge_command(&line).unwrap();
        assert_eq!((req, by.as_str()), (9, "Gemini 10"));
        assert_eq!(parsed, command);
    }

    #[test]
    fn every_bridge_command_round_trips() {
        for command in [
            BridgeCommand::Prompt { text: "hi".into() },
            BridgeCommand::Answer {
                approval: 4,
                allow: true,
            },
            BridgeCommand::Cancel,
            BridgeCommand::Steer {
                text: "also".into(),
            },
            BridgeCommand::SetMode {
                mode: "full-access".into(),
                confirm: Some(FULL_ACCESS_CONFIRM.into()),
            },
            BridgeCommand::SetModel {
                selection: "gpt-5.5".into(),
            },
            BridgeCommand::End,
        ] {
            let line = bridge_command_json(1, "phone", &command).to_string();
            let (_, _, parsed) =
                parse_bridge_command(&line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert_eq!(parsed, command);
        }
    }

    #[test]
    fn a_bridge_command_needs_a_device_and_nothing_extra() {
        for (text, wanted) in [
            (r#"{"v":1,"req":1,"type":"cancel"}"#, "Missing by"),
            (
                r#"{"v":1,"req":1,"type":"cancel","by":"p","session":"s"}"#,
                "session",
            ),
            (
                r#"{"v":1,"req":1,"type":"hello","by":"p"}"#,
                "Unknown message hello",
            ),
            (r#"{"v":2,"req":1,"type":"cancel","by":"p"}"#, "version 1"),
        ] {
            let error = parse_bridge_command(text).unwrap_err();
            assert!(error.contains(wanted), "{text}: {error}");
        }
    }

    #[test]
    fn the_frame_builders_match_the_schema() {
        let session = json!({
            "id": "1-2", "engine": "claude", "workspace": "/w", "mode": "ask", "phase": "idle",
            "approvals": 0, "from_phone": false,
        });
        let state = json!({
            "phase": "running", "queue": ["next"], "queued": 1,
            "goal": {"objective": "Ship", "status": "active", "turns": 1, "max": 5},
            "now": "Reading", "mode": "ask",
            "approvals": [{"id": 1, "summary": {"action": "Run a shell command"}}],
        });
        let catalog = json!({
            "modes": ["ask"], "current_mode": "ask",
            "models": [{"engine": "claude", "selection": "opus", "name": "Opus", "id": "claude-opus"}],
            "current": {"engine": "claude", "selection": "opus"},
        });
        let recent = json!({"engine": "codex", "workspace": "/w", "session": "s", "title": "t", "started": 5});
        let record =
            json!({"format": "octet-preview-1", "seq": 3, "type": "text", "data": {"text": "hi"}});
        for (frame, direction) in [
            (welcome("Gemini 10"), Direction::ToPhone),
            (paired("tok", "Gemini 10"), Direction::ToPhone),
            (sessions_frame(&[session]), Direction::ToPhone),
            (
                event_frame("1-2", "session-1.jsonl", 3, &record),
                Direction::ToPhone,
            ),
            (state_frame("1-2", &state), Direction::ToPhone),
            (catalog_frame("1-2", &catalog), Direction::ToPhone),
            (recent_frame(&[recent]), Direction::ToPhone),
            (ack(1, "Sent"), Direction::ToPhone),
            (refused(1, "A turn is running"), Direction::ToPhone),
            (bridge_state_frame(&state), Direction::FromBridge),
            (bridge_catalog_frame(&catalog), Direction::FromBridge),
            (ack(1, "Sent"), Direction::FromBridge),
            (refused(1, "no"), Direction::FromBridge),
        ] {
            let map = frame.as_object().unwrap();
            assert_eq!(map["v"], 1, "{frame}");
            let kind = map["type"].as_str().unwrap();
            let message = MESSAGES
                .iter()
                .find(|m| m.kind == kind && m.direction == direction)
                .unwrap();
            check_fields(map, message.fields, &["v", "type"], "")
                .unwrap_or_else(|e| panic!("{frame}: {e}"));
        }
    }

    #[test]
    fn a_frame_with_a_wrong_shape_fails_the_check() {
        let message = MESSAGES.iter().find(|m| m.kind == "sessions").unwrap();
        let bad: Value = json!({"v": 1, "type": "sessions", "sessions": [{"id": 5}]});
        assert!(
            check_fields(bad.as_object().unwrap(), message.fields, &["v", "type"], "").is_err()
        );
    }

    #[test]
    fn kinds_are_unique_within_a_direction() {
        for (n, a) in MESSAGES.iter().enumerate() {
            for b in &MESSAGES[n + 1..] {
                assert!(
                    !(a.kind == b.kind && a.direction == b.direction),
                    "{} twice",
                    a.kind
                );
            }
        }
    }

    #[test]
    fn the_typescript_has_every_message_and_union() {
        let ts = typescript();
        assert!(ts.starts_with("// Generated by octet-core (remote::protocol). Do not edit.\n"));
        assert!(ts.ends_with("}\n") || ts.ends_with(";\n"));
        assert!(!ts.ends_with("\n\n"));
        assert!(ts.contains("export const PROTOCOL_VERSION = 1;"));
        for union in ["ToPhone", "FromPhone", "ToBridge", "FromBridge"] {
            assert!(ts.contains(&format!("export type {union} =")), "{union}");
        }
        assert!(ts.contains("export interface Cursor {"));
        assert!(ts.contains("after?: Cursor;"));
        assert!(ts.contains("approvals: ApprovalInfo[];"));
        assert!(ts.contains("approval: number;"));
        assert!(ts.contains("type: \"set_mode\";"));
    }

    #[test]
    fn the_checked_in_typescript_is_current() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../apps/mobile/src/protocol.ts"
        );
        let wanted = typescript();
        if std::env::var_os("OCTET_WRITE_PROTOCOL").is_some() {
            std::fs::write(path, &wanted).unwrap();
        }
        let current = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            current, wanted,
            "run with OCTET_WRITE_PROTOCOL=1 to regenerate"
        );
    }
}
