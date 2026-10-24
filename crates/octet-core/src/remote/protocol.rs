//! The remote-control wire protocol, defined once: every message a terminal UI, the hub and a
//! phone exchange is a row in [`MESSAGES`], and the Rust parsers, the frame builders and the
//! app's TypeScript (`apps/mobile/src/protocol.ts`) all follow that table.
use serde_json::{Map, Value, json};
use std::fmt::Write as _;

/// The protocol version every message carries as `v`.
pub const VERSION: u64 = 1;
/// The most bytes one message may take (a 64 KiB prompt, JSON-escaped, with room). The hub's
/// `sessions` frame is bounded by it too.
pub const MAX_MESSAGE: usize = 96 * 1024;
/// The most rows one `sessions` frame lists: the most recently active sessions.
pub const MAX_SESSION_ROWS: usize = 200;
/// The most bytes one hub → bridge line may take: a phone's largest message, re-wrapped with the
/// device's name, fits with room.
pub const MAX_BRIDGE_LINE: usize = MAX_MESSAGE + 1024;
/// The longest path a `diff` may name, in characters.
pub const PATH_LIMIT: usize = 4096;
/// The longest commit message, in characters.
pub const COMMIT_MESSAGE_LIMIT: usize = 2000;
/// The refusal for a commit message that is empty, all space, over [`COMMIT_MESSAGE_LIMIT`]
/// characters or holds a NUL.
pub const NO_COMMIT_MESSAGE: &str = "Write a commit message";
/// The refusal for a `diff` path over [`PATH_LIMIT`] characters.
pub const PATH_TOO_LONG: &str = "The path is over 4,096 characters";
/// The refusal for a `diff` path that is empty or holds a NUL.
const NOT_A_PATH: &str = "Not a path";
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
    /// A string from the named closed set (a row of [`CHOICES`]). The parser takes any string, so
    /// a value outside the set is refused in the receiver's own words.
    Choice(&'static str),
    /// A list of strings from the named closed set.
    ChoiceList(&'static str),
    /// JSON `null` (a journal record with no data).
    Null,
    /// Any JSON object.
    Map,
    /// Any JSON list.
    List,
    /// A journal record's `{type, data}`, typed by [`EVENTS`].
    Event,
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
    /// Whether the key must be present but may be null (`optional` is then false).
    pub nullable: bool,
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

/// A closed set of strings, named for the TypeScript.
#[derive(Debug, Clone, Copy)]
pub struct Choices {
    /// The set's name (its TypeScript type).
    pub name: &'static str,
    /// Every value, in order.
    pub values: &'static [&'static str],
    /// One line saying what it is for.
    pub doc: &'static str,
}

/// One kind of journal record and the shape of its data.
#[derive(Debug, Clone, Copy)]
pub struct EventKind {
    /// The record's `type`.
    pub kind: &'static str,
    /// What its `data` is.
    pub data: Ty,
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
        nullable: false,
    }
}
const fn opt(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        optional: true,
        nullable: false,
    }
}
/// A field that must be present but may be null.
const fn req_null(name: &'static str, ty: Ty) -> Field {
    Field {
        name,
        ty,
        optional: false,
        nullable: true,
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
use Ty::{Bool, Choice, ChoiceList, Json, List, Null, Object, ObjectList, Str, StrList, U64};

/// The closed sets of strings, in the order the TypeScript declares them.
pub static CHOICES: &[Choices] = &[
    Choices {
        name: "Engine",
        values: &["claude", "codex", "demo", "opencode"],
        doc: "A provider.",
    },
    Choices {
        name: "Mode",
        values: &["ask", "accept-edits", "auto", "full-access"],
        doc: "A permission mode.",
    },
    Choices {
        name: "Phase",
        values: &["connecting", "idle", "running", "stopped"],
        doc: "Where a session's connection is.",
    },
    Choices {
        name: "ReplayFrom",
        values: &["cursor", "start"],
        doc: "Where a replay starts: after the phone's cursor, or at the start of the session.",
    },
    Choices {
        name: "ToolPhase",
        values: &["running", "done", "failed"],
        doc: "Where a tool call is.",
    },
    Choices {
        name: "CommandName",
        values: &["new", "session", "help", "export"],
        doc: "A desk command a phone may run: a closed set, none of which takes an argument.",
    },
    Choices {
        name: "FileStatus",
        values: &["M", "A", "D", "R", "?", "U"],
        doc: "How a file changed: modified, added, deleted, renamed, untracked or unmerged.",
    },
    Choices {
        name: "RefusalCode",
        values: &[
            "unpaired",
            "revoked",
            "rate_limited",
            "not_allowed",
            "turn_open",
            "not_connected",
            "invalid",
            "unavailable",
            "busy",
            "too_large",
            "timeout",
        ],
        doc: "Why a command was refused, for a client to act on; `reason` is the words to show.",
    },
];

/// Every kind of journal record (the session's `record()`, and the journal's `session` header),
/// with its data. The TypeScript adds `UnknownJournalEvent` for kinds a later version adds.
pub static EVENTS: &[EventKind] = &[
    EventKind {
        kind: "text",
        data: Str,
    },
    EventKind {
        kind: "user",
        data: Str,
    },
    EventKind {
        kind: "finished",
        data: Str,
    },
    EventKind {
        kind: "mode",
        data: Str,
    },
    EventKind {
        kind: "model_selected",
        data: Str,
    },
    EventKind {
        kind: "effort",
        data: Json,
    },
    EventKind {
        kind: "usage",
        data: Str,
    },
    EventKind {
        kind: "notice",
        data: Str,
    },
    EventKind {
        kind: "error",
        data: Str,
    },
    EventKind {
        kind: "tool",
        data: Object("ToolData"),
    },
    EventKind {
        kind: "approval",
        data: Object("ApprovalData"),
    },
    EventKind {
        kind: "approval_closed",
        data: U64,
    },
    EventKind {
        kind: "started",
        data: Null,
    },
    EventKind {
        kind: "stopped",
        data: Null,
    },
    EventKind {
        kind: "hook",
        data: Object("HookData"),
    },
    EventKind {
        kind: "ready",
        data: Str,
    },
    EventKind {
        kind: "session",
        data: Ty::Map,
    },
    EventKind {
        kind: "models",
        data: List,
    },
    EventKind {
        kind: "skills",
        data: List,
    },
];

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
            req("engine", Choice("Engine")),
            opt("model", Str),
            req("workspace", Str),
            req("mode", Choice("Mode")),
            req("phase", Choice("Phase")),
            req("approvals", U64),
            opt("last_activity", U64),
            opt("goal", Str),
            opt("now", Str),
            req("from_phone", Bool),
            opt("tmux", Str),
            opt("approval", Object("ApprovalInfo")),
            opt("usage", Str),
            opt("question", Bool),
        ],
        doc: "One live session; `last_activity` is Unix seconds, `goal` a one-liner, `now` the \
              NOW line, `tmux` the tmux session a phone started it in, `approval` the first \
              approval waiting (the count is `approvals`), `usage` the latest usage line, and \
              `question` true when the last finished turn's reply ended on a `?` (a heuristic; \
              cleared by the next prompt).",
    },
    Shape {
        name: "PushKinds",
        fields: &[
            req("approvals", Bool),
            req("errors", Bool),
            req("turns", Bool),
        ],
        doc: "What a device is pushed: an approval waiting, a session error, a finished turn.",
    },
    Shape {
        name: "ApprovalSummary",
        fields: &[
            req("action", Str),
            opt("command", Str),
            req("files", StrList),
            opt("cwd", Str),
            req("scope", Str),
        ],
        doc: "What an approval would do, as the dialog leads with it.",
    },
    Shape {
        name: "ToolData",
        fields: &[
            req("id", Str),
            req("name", Str),
            req("target", Str),
            req("phase", Choice("ToolPhase")),
            req("detail", Str),
        ],
        doc: "A `tool` record: one tool call or its result, as a preview.",
    },
    Shape {
        name: "ApprovalData",
        fields: &[
            req("id", U64),
            req("detail", Str),
            req("summary", Object("ApprovalSummary")),
        ],
        doc: "An `approval` record: the vendor waits for an answer.",
    },
    Shape {
        name: "HookData",
        fields: &[
            req("name", Str),
            req("event", Str),
            req("outcome", Str),
            req("detail", Str),
            opt("decision", Str),
        ],
        doc: "A `hook` record: a plugin or settings hook ran.",
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
        fields: &[req("id", U64), req("summary", Object("ApprovalSummary"))],
        doc: "An approval waiting for an answer, with its summary as the journal records it.",
    },
    Shape {
        name: "StateInfo",
        fields: &[
            req("phase", Choice("Phase")),
            req("queue", StrList),
            req("queued", U64),
            opt("goal", Object("GoalInfo")),
            opt("now", Str),
            req("mode", Choice("Mode")),
            req("approvals", ObjectList("ApprovalInfo")),
        ],
        doc: "What only the terminal UI knows: queue, goal, NOW line, mode and pending approvals.",
    },
    Shape {
        name: "ModelInfo",
        fields: &[
            req("engine", Choice("Engine")),
            req("selection", Str),
            req("name", Str),
            req("id", Str),
        ],
        doc: "One model a session can switch to.",
    },
    Shape {
        name: "ModelRef",
        fields: &[req("engine", Choice("Engine")), req("selection", Str)],
        doc: "The model a session is using now.",
    },
    Shape {
        name: "CatalogInfo",
        fields: &[
            req("modes", ChoiceList("Mode")),
            req("current_mode", Choice("Mode")),
            req("models", ObjectList("ModelInfo")),
            opt("current", Object("ModelRef")),
        ],
        doc: "The modes, and every provider's models, with the current ones marked.",
    },
    Shape {
        name: "RecentInfo",
        fields: &[
            req("engine", Choice("Engine")),
            req("workspace", Str),
            req("session", Str),
            req("title", Str),
            opt("model", Str),
            req("started", U64),
        ],
        doc: "One resumable session; `started` is Unix seconds.",
    },
    Shape {
        name: "FileChange",
        fields: &[
            req("path", Str),
            req("add", U64),
            req("del", U64),
            req("binary", Bool),
            req("status", Choice("FileStatus")),
        ],
        doc: "One changed file against HEAD: its path from the repository's top (git's bytes, \
              read as UTF-8), lines added and deleted (0 for a binary file), and how it changed.",
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
        "The live sessions, oldest first: at most `MAX_SESSION_ROWS`, the most recently active, \
         in a frame of at most `MAX_MESSAGE` bytes. Past that, rows lose their `approval`, \
         `usage`, `now` and `goal`, in that order and least recently active first, and then the \
         least recently active rows go. Sent when a row changes, at most four times a second.",
    ),
    message(
        "subscribed",
        ToPhone,
        &[
            req("session", Str),
            req("from", Choice("ReplayFrom")),
            req("journals", StrList),
        ],
        "A replay begins: after the cursor, or from the start of the session's first journal when \
         there was no cursor or the hub does not know its journal; `journals` lists every \
         journal, oldest first.",
    ),
    message(
        "event",
        ToPhone,
        &[
            req("session", Str),
            req("journal", Str),
            req("seq", U64),
            req("event", Ty::Event),
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
        &[req("req", U64), req("items", ObjectList("RecentInfo"))],
        "Recent sessions that can be resumed, answering the `recent` command `req`.",
    ),
    message(
        "changes",
        ToPhone,
        &[
            req("req", U64),
            opt("branch", Str),
            opt("upstream", Str),
            req("ahead", U64),
            req("behind", U64),
            req("files", ObjectList("FileChange")),
            req("more", U64),
        ],
        "What the session's workspace changed against HEAD, answering the `changes` command \
         `req`: the branch (none when detached), its upstream and how far ahead and behind it \
         is, at most 500 files, and `more` counting the rest.",
    ),
    message(
        "diff",
        ToPhone,
        &[
            req("req", U64),
            req("path", Str),
            req("text", Str),
            req("clipped", Bool),
            req("binary", Bool),
        ],
        "One file's diff against HEAD, answering the `diff` command `req`; `clipped` when the \
         text was cut at 64 KiB or 2,000 lines, `binary` for a binary file.",
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
        &[
            req("req", U64),
            req("reason", Str),
            opt("code", Choice("RefusalCode")),
        ],
        "A command was refused, with the reason.",
    ),
    message(
        "hello",
        FromPhone,
        &[req("token", Str), opt("background", Bool)],
        "Connect with a device token. `background: true` marks a connection the app makes from \
         the background (to hand a Live Activity token over, or to send a Deny): it is not the \
         app in front. Left out, the connection is the app in front.",
    ),
    message(
        "pair",
        FromPhone,
        &[req("secret", Str), req("name", Str), req("platform", Str)],
        "Pair a new device with a pairing secret.",
    ),
    message(
        "register_push",
        FromPhone,
        &[req_null("token", Str), opt("kinds", Object("PushKinds"))],
        "Set the Expo push token notifications go to (`ExponentPushToken[..]` or \
         `ExpoPushToken[..]`), or turn them off with `null`; leaving `token` out is refused. \
         `kinds` says what to push; left out, the hub keeps what it has (approvals and errors \
         for a new device).",
    ),
    message(
        "register_activity",
        FromPhone,
        &[opt("token", Str), opt("start_token", Str)],
        "Say the ActivityKit tokens the phone has now, for the hub's Live Activity pushes: \
         `token`, the running activity's push token, and `start_token`, the push-to-start \
         token, each 1 to 1024 lowercase hex digits. Either may be left out, as the phone has \
         none, but not both. Each registration replaces both: a token left out is one the hub \
         no longer has.",
    ),
    message(
        "clear_activity",
        FromPhone,
        &[],
        "Forget the ActivityKit tokens the hub had, so it no longer starts or updates a Live \
         Activity on this phone.",
    ),
    message(
        "ping",
        FromPhone,
        &[],
        "Ask the hub to answer, to tell a live connection from a dead one.",
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
        &[
            req("session", Str),
            req("mode", Choice("Mode")),
            opt("confirm", Str),
        ],
        "Change the permission mode; full-access needs `confirm`.",
    ),
    message(
        "set_model",
        FromPhone,
        &[req("session", Str), req("selection", Str)],
        "Switch the model.",
    ),
    message(
        "command",
        FromPhone,
        &[req("session", Str), req("name", Choice("CommandName"))],
        "Run one of the desk's own commands: `new`, `session`, `help` or `export` (the \
         journal, to the desk's default file).",
    ),
    message(
        "recent",
        FromPhone,
        &[opt("workspace", Str)],
        "List recent sessions, optionally of one workspace.",
    ),
    message(
        "changes",
        FromPhone,
        &[req("session", Str)],
        "List what the session's workspace changed against HEAD.",
    ),
    message(
        "diff",
        FromPhone,
        &[req("session", Str), req("path", Str)],
        "One file's diff; `path` must be one the last `changes` answer on this connection \
         listed for the session.",
    ),
    message(
        "commit",
        FromPhone,
        &[req("session", Str), req("message", Str)],
        "Stage everything and commit it with `message` (1 to 2,000 characters); the \
         repository's hooks run. Refused while a turn runs or an approval waits.",
    ),
    message(
        "start",
        FromPhone,
        &[
            req("engine", Choice("Engine")),
            req("workspace", Str),
            opt("resume", Str),
            opt("model", Str),
            opt("mode", Choice("Mode")),
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
        &[
            req("by", Str),
            req("mode", Choice("Mode")),
            opt("confirm", Str),
        ],
        "Change the mode.",
    ),
    message(
        "set_model",
        ToBridge,
        &[req("by", Str), req("selection", Str)],
        "Switch the model.",
    ),
    message(
        "command",
        ToBridge,
        &[req("by", Str), req("name", Choice("CommandName"))],
        "Run one of the desk's own commands.",
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
        &[
            req("req", U64),
            req("reason", Str),
            opt("code", Choice("RefusalCode")),
        ],
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

/// Why a command was refused, for a client to act on; the reason stays the words to show. The
/// spellings are [`CHOICES`]' `RefusalCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// The device is not paired, or has not said hello.
    Unpaired,
    /// The device was unpaired.
    Revoked,
    /// The device sent too many commands; it may try again in a minute.
    RateLimited,
    /// The command is understood but not allowed (a workspace, full access unconfirmed).
    NotAllowed,
    /// A turn is running; finish or cancel it first.
    TurnOpen,
    /// The session is not connected to its vendor, or the hub to it.
    NotConnected,
    /// The command is malformed, or names something that is not there to act on.
    Invalid,
    /// The session ended, or what the command needs is missing.
    Unavailable,
    /// Something is in the way for now (the desk, a full queue, a scan, the session cap).
    Busy,
    /// The text is over its limit.
    TooLarge,
    /// No answer came in time.
    Timeout,
}

impl Code {
    /// Every code, in [`CHOICES`]' order.
    pub const ALL: [Code; 11] = [
        Code::Unpaired,
        Code::Revoked,
        Code::RateLimited,
        Code::NotAllowed,
        Code::TurnOpen,
        Code::NotConnected,
        Code::Invalid,
        Code::Unavailable,
        Code::Busy,
        Code::TooLarge,
        Code::Timeout,
    ];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Unpaired => "unpaired",
            Code::Revoked => "revoked",
            Code::RateLimited => "rate_limited",
            Code::NotAllowed => "not_allowed",
            Code::TurnOpen => "turn_open",
            Code::NotConnected => "not_connected",
            Code::Invalid => "invalid",
            Code::Unavailable => "unavailable",
            Code::Busy => "busy",
            Code::TooLarge => "too_large",
            Code::Timeout => "timeout",
        }
    }

    /// The code spelled `text`, if there is one.
    pub fn parse(text: &str) -> Option<Code> {
        Code::ALL.into_iter().find(|code| code.as_str() == text)
    }
}

/// Text that must never be shown, such as a device token: its Debug output says only that it
/// was redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The text itself, for the one place that checks it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A desk command a phone may run. The set is closed and none takes an argument, so no text
/// from the phone reaches a path or a shell. The spellings are [`CHOICES`]' `CommandName`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandName {
    /// `/new`: start a fresh conversation.
    New,
    /// `/session`: the session's details.
    Session,
    /// `/help`: what the phone can run.
    Help,
    /// `/export`: write the journal to the desk's default file.
    Export,
}

impl CommandName {
    /// Every name, in [`CHOICES`]' order.
    pub const ALL: [CommandName; 4] = [
        CommandName::New,
        CommandName::Session,
        CommandName::Help,
        CommandName::Export,
    ];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            CommandName::New => "new",
            CommandName::Session => "session",
            CommandName::Help => "help",
            CommandName::Export => "export",
        }
    }

    /// The name spelled `text`, if there is one.
    pub fn parse(text: &str) -> Option<CommandName> {
        CommandName::ALL
            .into_iter()
            .find(|name| name.as_str() == text)
    }
}

/// How a file changed, as a `FileChange` says it. The spellings are [`CHOICES`]' `FileStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Modified (or its type changed).
    Modified,
    /// Added to the index.
    Added,
    /// Deleted.
    Deleted,
    /// Renamed or copied.
    Renamed,
    /// Not tracked yet.
    Untracked,
    /// Unmerged: a conflict.
    Unmerged,
}

impl FileStatus {
    /// Every status, in [`CHOICES`]' order.
    pub const ALL: [FileStatus; 6] = [
        FileStatus::Modified,
        FileStatus::Added,
        FileStatus::Deleted,
        FileStatus::Renamed,
        FileStatus::Untracked,
        FileStatus::Unmerged,
    ];

    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Modified => "M",
            FileStatus::Added => "A",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Untracked => "?",
            FileStatus::Unmerged => "U",
        }
    }

    /// The status spelled `text`, if there is one.
    pub fn parse(text: &str) -> Option<FileStatus> {
        FileStatus::ALL.into_iter().find(|s| s.as_str() == text)
    }
}

/// One changed file of a [`ChangesInfo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Its path from the repository's top.
    pub path: String,
    /// Lines added.
    pub add: u64,
    /// Lines deleted.
    pub del: u64,
    /// Whether git sees it as binary.
    pub binary: bool,
    /// How it changed.
    pub status: FileStatus,
}

/// What a workspace changed, for the `changes` frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangesInfo {
    /// The branch; none when HEAD is detached.
    pub branch: Option<String>,
    /// The branch's upstream, if it has one.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: u64,
    /// Commits behind it.
    pub behind: u64,
    /// The files listed.
    pub files: Vec<FileChange>,
    /// The changed files left out.
    pub more: u64,
}

/// One file's diff, for the `diff` frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffInfo {
    /// The path, as the `changes` frame listed it.
    pub path: String,
    /// The diff text.
    pub text: String,
    /// Whether the text was cut short.
    pub clipped: bool,
    /// Whether the file is binary.
    pub binary: bool,
}

/// What a device is pushed, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushKinds {
    /// An approval waiting.
    pub approvals: bool,
    /// A session error.
    pub errors: bool,
    /// A finished turn.
    pub turns: bool,
}

/// A device that never said: approvals and errors, not every finished turn.
impl Default for PushKinds {
    fn default() -> Self {
        Self {
            approvals: true,
            errors: true,
            turns: false,
        }
    }
}

/// The refusal for a `set_mode` whose mode is not one of the modes; it names none the phone sent.
const UNKNOWN_MODE: &str = "Unknown mode; use ask, accept-edits, auto or full-access";

/// A phone → hub command, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneCommand {
    /// Connect with a device token.
    Hello {
        /// The device token.
        token: Secret,
        /// Whether this connection is the app in the background (handing a Live Activity token
        /// over, or sending a Deny) rather than the app in front; left out, it is in front.
        background: bool,
    },
    /// Pair a new device.
    Pair {
        /// The single-use pairing secret.
        secret: Secret,
        /// The device's name.
        name: String,
        /// `ios` or `android`.
        platform: String,
    },
    /// Set the Expo push token notifications go to, or turn them off.
    RegisterPush {
        /// The checked Expo push token; `None` turns notifications off.
        token: Option<Secret>,
        /// What to push; `None` keeps what the hub has.
        kinds: Option<PushKinds>,
    },
    /// Say the `ActivityKit` tokens the phone has now, replacing both; at least one is there.
    RegisterActivity {
        /// The checked push token of the running activity; `None` when none runs.
        token: Option<Secret>,
        /// The checked push-to-start token; `None` when the phone has none.
        start_token: Option<Secret>,
    },
    /// Forget the `ActivityKit` tokens the hub had for this device.
    ClearActivity,
    /// Answer with an ack, to show the connection is alive.
    Ping,
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
    /// Run one of the desk's own commands.
    Command {
        /// The session id.
        session: String,
        /// Which command.
        name: CommandName,
    },
    /// List recent sessions.
    Recent {
        /// Only this workspace's.
        workspace: Option<String>,
    },
    /// List what the session's workspace changed.
    Changes {
        /// The session id.
        session: String,
    },
    /// One listed file's diff.
    Diff {
        /// The session id.
        session: String,
        /// The path, as the last `changes` listed it.
        path: String,
    },
    /// Commit everything.
    Commit {
        /// The session id.
        session: String,
        /// The commit message.
        message: String,
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
    /// Run one of the desk's own commands.
    Command {
        /// Which command.
        name: CommandName,
    },
    /// Quit the terminal UI.
    End,
}

/// The keys every command carries beside its `fields`.
const COMMON: &[&str] = &["v", "req", "type"];

fn type_name(ty: Ty) -> &'static str {
    match ty {
        Ty::Str | Ty::Choice(_) => "string",
        Ty::U64 => "non-negative number",
        Ty::Bool => "boolean",
        Ty::Json => "JSON value",
        Ty::StrList | Ty::ChoiceList(_) => "list of strings",
        Ty::Null => "null",
        Ty::List => "list",
        Ty::Map | Ty::Event | Ty::Object(_) => "object",
        Ty::ObjectList(_) => "list of objects",
    }
}

fn shape(name: &str) -> &'static [Field] {
    OBJECTS
        .iter()
        .find(|s| s.name == name)
        .map_or(&[], |s| s.fields)
}

/// What every journal event is: its kind, and data that the kind decides (null for some).
const EVENT_FIELDS: &[Field] = &[req("type", Str), opt("data", Json)];

/// Checks `value` against `ty`; `name` is the dotted path for messages.
fn check_value(value: &Value, ty: Ty, name: &str) -> Result<(), String> {
    let wrong = || format!("{name} must be a {}", type_name(ty));
    match ty {
        Ty::Json => Ok(()),
        Ty::Str | Ty::Choice(_) if value.is_string() => Ok(()),
        Ty::U64 if value.is_u64() => Ok(()),
        Ty::Bool if value.is_boolean() => Ok(()),
        Ty::Null if value.is_null() => Ok(()),
        Ty::Map if value.is_object() => Ok(()),
        Ty::List if value.is_array() => Ok(()),
        Ty::Event => {
            let map = value.as_object().ok_or_else(wrong)?;
            check_fields(map, EVENT_FIELDS, &[], name)
        }
        Ty::StrList | Ty::ChoiceList(_)
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
            Some(Value::Null) if field.nullable => {}
            None | Some(Value::Null) => return Err(format!("Missing {}", at(field.name))),
            Some(value) => check_value(value, field.ty, &at(field.name))?,
        }
    }
    match map
        .keys()
        .find(|k| !common.contains(&k.as_str()) && !fields.iter().any(|f| f.name == k.as_str()))
    {
        Some(key) => Err(format!("Unexpected field {}", at(&clip(key)))),
        None => Ok(()),
    }
}

/// Parses and checks one inbound message of `direction`, at most `limit` bytes: its `req`, its
/// definition and its object.
fn parse_message(
    text: &str,
    direction: Direction,
    limit: usize,
) -> Result<(u64, &'static Message, Map<String, Value>), String> {
    if text.len() > limit {
        return Err(format!("Message too large; the limit is {limit} bytes"));
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
        .ok_or_else(|| format!("Unknown message {}", clip(kind)))?;
    check_fields(&map, message.fields, COMMON, "")?;
    Ok((req, message, map))
}

/// At most 64 characters of an attacker-supplied name, for an error message.
fn clip(name: &str) -> String {
    const LIMIT: usize = 64;
    if name.chars().count() <= LIMIT {
        return name.to_owned();
    }
    name.chars()
        .take(LIMIT)
        .chain(std::iter::once('…'))
        .collect()
}

/// The `req` of a message, so a refusal can name the request even when the rest is malformed;
/// 0 when the text is too large, not a JSON object, or has no whole-number `req`.
pub fn req_of(text: &str) -> u64 {
    if text.len() > MAX_MESSAGE {
        return 0;
    }
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("req")?.as_u64())
        .unwrap_or_default()
}

/// Whether a mode change (or start) may go ahead: any mode but `full-access`, or `full-access`
/// with exactly [`FULL_ACCESS_CONFIRM`] typed.
pub fn full_access_confirmed(mode: &str, confirm: Option<&str>) -> bool {
    mode != "full-access" || confirm == Some(FULL_ACCESS_CONFIRM)
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

/// Whether `token` is an Expo push token: `ExpoPushToken[..]` or `ExponentPushToken[..]` around
/// 1 to 200 letters, digits, `_` or `-`.
pub fn is_push_token(token: &str) -> bool {
    let Some(inner) = token
        .strip_prefix("ExpoPushToken[")
        .or_else(|| token.strip_prefix("ExponentPushToken["))
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    (1..=200).contains(&inner.len())
        && inner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The most hex digits an `ActivityKit` token may have. Apple doesn't fix the length (iOS 26.5
/// issues 128 bytes, 256 digits); 1024 digits is 512 bytes, far above any token seen.
pub const ACTIVITY_TOKEN_DIGITS: usize = 1024;

/// Whether `token` is an `ActivityKit` token as the phone sends it: 1 to
/// [`ACTIVITY_TOKEN_DIGITS`] lowercase hex digits.
pub fn is_activity_token(token: &str) -> bool {
    (1..=ACTIVITY_TOKEN_DIGITS).contains(&token.len())
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The `ActivityKit` token `key` of a `register_activity`, checked; `None` when it is left out or
/// null. A refusal never echoes the text.
fn activity_token(map: &Map<String, Value>, key: &str) -> Result<Option<Secret>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_str() {
            Some(token) if is_activity_token(token) => Ok(Some(Secret(token.to_owned()))),
            _ => Err(format!(
                "{key} must be 1 to {ACTIVITY_TOKEN_DIGITS} lowercase hex digits"
            )),
        },
    }
}

/// The `token` of a `register_push`: a checked Expo push token, or `None` for `null`. A missing
/// `token` is refused, so that turning notifications off is always said.
fn push_token(map: &Map<String, Value>) -> Result<Option<Secret>, String> {
    match map.get("token") {
        None => Err("Missing token".into()),
        Some(Value::Null) => Ok(None),
        Some(value) => match value.as_str() {
            Some(token) if is_push_token(token) => Ok(Some(Secret(token.to_owned()))),
            _ => Err("token must be an Expo push token or null".into()),
        },
    }
}

/// The `name` of a `command`: one of [`CommandName`], or a refusal that does not echo the text.
fn command_name(map: &Map<String, Value>) -> Result<CommandName, String> {
    map.get("name")
        .and_then(Value::as_str)
        .and_then(CommandName::parse)
        .ok_or_else(|| "name must be new, session, help or export".into())
}

/// The `path` of a `diff`: 1 to [`PATH_LIMIT`] characters with no NUL. A refusal never echoes
/// the text.
fn diff_path(map: &Map<String, Value>) -> Result<String, String> {
    let path = text(map, "path");
    if path.chars().count() > PATH_LIMIT {
        return Err(PATH_TOO_LONG.into());
    }
    if path.is_empty() || path.contains('\0') {
        return Err(NOT_A_PATH.into());
    }
    Ok(path)
}

/// The `message` of a `commit`: 1 to [`COMMIT_MESSAGE_LIMIT`] characters, not all space, with no
/// NUL (git would cut the message there). A refusal never echoes the text.
fn commit_message(map: &Map<String, Value>) -> Result<String, String> {
    let message = text(map, "message");
    if message.trim().is_empty()
        || message.contains('\0')
        || message.chars().count() > COMMIT_MESSAGE_LIMIT
    {
        return Err(NO_COMMIT_MESSAGE.into());
    }
    Ok(message)
}

/// A `set_mode`, whose mode must be one the terminal UI takes: nothing else reaches a session.
fn set_mode(m: &Map<String, Value>) -> Result<PhoneCommand, String> {
    let mode = text(m, "mode");
    if crate::Mode::parse(&mode).is_none() {
        return Err(UNKNOWN_MODE.into());
    }
    Ok(PhoneCommand::SetMode {
        session: text(m, "session"),
        mode,
        confirm: maybe(m, "confirm"),
    })
}

/// Parses one phone message: `(req, command)`, or the reason it was refused.
///
/// # Errors
///
/// The reason, in words for the phone: a message too large, not JSON, of another version, of an
/// unknown type, or with a field missing, mistyped or unexpected.
pub fn parse_phone(text_in: &str) -> Result<(u64, PhoneCommand), String> {
    let (req, message, map) = parse_message(text_in, FromPhone, MAX_MESSAGE)?;
    let m = &map;
    let command = match message.kind {
        "hello" => PhoneCommand::Hello {
            token: Secret(text(m, "token")),
            background: flag(m, "background"),
        },
        "pair" => PhoneCommand::Pair {
            secret: Secret(text(m, "secret")),
            name: text(m, "name"),
            platform: text(m, "platform"),
        },
        "register_push" => PhoneCommand::RegisterPush {
            token: push_token(m)?,
            kinds: m
                .get("kinds")
                .and_then(Value::as_object)
                .map(|k| PushKinds {
                    approvals: flag(k, "approvals"),
                    errors: flag(k, "errors"),
                    turns: flag(k, "turns"),
                }),
        },
        "register_activity" => match (
            activity_token(m, "token")?,
            activity_token(m, "start_token")?,
        ) {
            (None, None) => return Err("register_activity needs token or start_token".into()),
            (token, start_token) => PhoneCommand::RegisterActivity { token, start_token },
        },
        "clear_activity" => PhoneCommand::ClearActivity,
        "ping" => PhoneCommand::Ping,
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
        "set_mode" => set_mode(m)?,
        "set_model" => PhoneCommand::SetModel {
            session: text(m, "session"),
            selection: text(m, "selection"),
        },
        "command" => PhoneCommand::Command {
            session: text(m, "session"),
            name: command_name(m)?,
        },
        "recent" => PhoneCommand::Recent {
            workspace: maybe(m, "workspace"),
        },
        "changes" => PhoneCommand::Changes {
            session: text(m, "session"),
        },
        "diff" => PhoneCommand::Diff {
            session: text(m, "session"),
            path: diff_path(m)?,
        },
        "commit" => PhoneCommand::Commit {
            session: text(m, "session"),
            message: commit_message(m)?,
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
        other => return Err(format!("Unknown message {}", clip(other))),
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
        BridgeCommand::Command { name } => ("command", json!({ "name": name.as_str() })),
        BridgeCommand::End => ("end", json!({})),
    };
    let mut line = json!({ "v": VERSION, "req": req, "type": kind, "by": by });
    if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    line
}

/// Parses one hub → bridge line of at most [`MAX_BRIDGE_LINE`] bytes: `(req, device, command)`,
/// or the reason it was refused.
///
/// # Errors
///
/// The reason: a line too large, not JSON, of another version, of an unknown type, or with a
/// field missing, mistyped or unexpected.
pub fn parse_bridge_command(line: &str) -> Result<(u64, String, BridgeCommand), String> {
    let (req, message, map) = parse_message(line, ToBridge, MAX_BRIDGE_LINE)?;
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
        "command" => BridgeCommand::Command {
            name: command_name(m)?,
        },
        "end" => BridgeCommand::End,
        other => return Err(format!("Unknown message {}", clip(other))),
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
/// The `subscribed` frame: session `session` replays after the phone's cursor (`from_cursor`)
/// or from its start, through `journals`.
pub fn subscribed_frame(session: &str, from_cursor: bool, journals: &[String]) -> Value {
    let from = if from_cursor { "cursor" } else { "start" };
    frame(
        "subscribed",
        json!({ "session": session, "from": from, "journals": journals }),
    )
}
/// The `event` frame for the journal record `seq` of `journal`: `kind` and `data` are the
/// record's `type` and `data` (its `format` and `seq` stay out; `seq` rides on the frame).
pub fn event_frame(session: &str, journal: &str, seq: u64, kind: &str, data: &Value) -> Value {
    frame(
        "event",
        json!({
            "session": session, "journal": journal, "seq": seq,
            "event": { "type": kind, "data": data },
        }),
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
/// The `recent` frame answering the command `req`: each row is a `RecentInfo`.
pub fn recent_frame(req: u64, items: &[Value]) -> Value {
    frame("recent", json!({ "req": req, "items": items }))
}
/// The `changes` frame answering the command `req`.
pub fn changes_frame(req: u64, info: &ChangesInfo) -> Value {
    let files: Vec<Value> = info
        .files
        .iter()
        .map(|file| {
            json!({
                "path": file.path, "add": file.add, "del": file.del, "binary": file.binary,
                "status": file.status.as_str(),
            })
        })
        .collect();
    let mut fields = json!({
        "req": req, "ahead": info.ahead, "behind": info.behind, "files": files,
        "more": info.more,
    });
    if let Some(branch) = &info.branch {
        fields["branch"] = json!(branch);
    }
    if let Some(upstream) = &info.upstream {
        fields["upstream"] = json!(upstream);
    }
    frame("changes", fields)
}
/// The `diff` frame answering the command `req`.
pub fn diff_frame(req: u64, info: &DiffInfo) -> Value {
    frame(
        "diff",
        json!({
            "req": req, "path": info.path, "text": info.text, "clipped": info.clipped,
            "binary": info.binary,
        }),
    )
}
/// The `ack` frame (hub → phone, or bridge → hub) for the command `req`.
pub fn ack(req: u64, message: &str) -> Value {
    frame("ack", json!({ "req": req, "message": message }))
}
/// The `refused` frame (hub → phone, or bridge → hub) for the command `req`, with its code and
/// the reason.
pub fn refused(req: u64, code: Code, reason: &str) -> Value {
    frame(
        "refused",
        json!({ "req": req, "code": code.as_str(), "reason": reason }),
    )
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
    // `command` would read `CommandCommand`.
    let kind = if message.kind == "command" {
        "Run".to_owned()
    } else {
        pascal(message.kind)
    };
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
        Ty::Null => "null".into(),
        Ty::Map => "Record<string, unknown>".into(),
        Ty::List => "unknown[]".into(),
        Ty::Event => "JournalEvent".into(),
        Ty::Choice(name) | Ty::Object(name) => name.into(),
        Ty::ChoiceList(name) | Ty::ObjectList(name) => format!("{name}[]"),
    }
}

/// The fields of an interface; an optional or nullable one may also be null, as the parser takes it.
fn ts_fields(out: &mut String, fields: &[Field]) {
    for field in fields {
        let (mark, null) = match (field.optional, field.nullable) {
            (true, _) => ("?", " | null"),
            (false, true) => ("", " | null"),
            (false, false) => ("", ""),
        };
        let _ = writeln!(out, "  {}{mark}: {}{null};", field.name, ts_type(field.ty));
    }
}

/// The journal event types: [`KnownJournalEvent`] (one member per record kind), the shape of any
/// other kind, their union, the list of known kinds and the guard that narrows to the known ones.
///
/// The catch-all is a type of its own: inside one union it would absorb every discriminant, and
/// `e.type === "text"` would not narrow `e.data`.
fn ts_events(out: &mut String) {
    out.push_str(
        "\n/** A journal record of a kind this version knows; the frame carries its `seq`. */\n",
    );
    out.push_str("export type KnownJournalEvent =\n");
    let members: Vec<String> = EVENTS
        .iter()
        .map(|event| {
            format!(
                "  | {{ type: \"{}\"; data: {} }}",
                event.kind,
                ts_type(event.data)
            )
        })
        .collect();
    out.push_str(&members.join("\n"));
    out.push_str(";\n");
    out.push_str("\n/** A journal record of a kind a later version added. */\n");
    out.push_str("export interface UnknownJournalEvent {\n  type: string;\n  data: unknown;\n}\n");
    out.push_str("\n/** Any journal record; narrow it with `isKnownEvent`. */\n");
    out.push_str("export type JournalEvent = KnownJournalEvent | UnknownJournalEvent;\n");
    let kinds: Vec<String> = EVENTS.iter().map(|e| format!("\"{}\"", e.kind)).collect();
    out.push_str("\n/** Every record kind `KnownJournalEvent` types. */\n");
    let _ = writeln!(
        out,
        "export const KNOWN_JOURNAL_KINDS: readonly string[] = [{}];",
        kinds.join(", ")
    );
    out.push_str("\n/** Whether `e` is a record of a kind this version knows. */\n");
    out.push_str("export function isKnownEvent(e: JournalEvent): e is KnownJournalEvent {\n");
    out.push_str("  return KNOWN_JOURNAL_KINDS.includes(e.type);\n}\n");
}

/// The checked-in TypeScript for the app, generated from [`MESSAGES`] and [`OBJECTS`].
pub fn typescript() -> String {
    let mut out = String::from("// Generated by octet-core (remote::protocol). Do not edit.\n");
    let _ = writeln!(out, "export const PROTOCOL_VERSION = {VERSION};");
    let _ = writeln!(
        out,
        "\n/** The most bytes one message may take; the hub keeps a `sessions` frame within it. */\nexport const MAX_MESSAGE = {MAX_MESSAGE};"
    );
    let _ = writeln!(
        out,
        "\n/** The most rows one `sessions` frame lists: the most recently active sessions. */\nexport const MAX_SESSION_ROWS = {MAX_SESSION_ROWS};"
    );
    for choices in CHOICES {
        let values: Vec<String> = choices.values.iter().map(|v| format!("\"{v}\"")).collect();
        let _ = writeln!(
            out,
            "\n/** {} */\nexport type {} = {};",
            choices.doc,
            choices.name,
            values.join(" | ")
        );
    }
    ts_events(&mut out);
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
            r#"{"v":1,"req":15,"type":"command","session":"12-34","name":"new"}"#,
            r#"{"v":1,"req":16,"type":"changes","session":"12-34"}"#,
            r#"{"v":1,"req":17,"type":"diff","session":"12-34","path":"src/a b.rs"}"#,
            r#"{"v":1,"req":18,"type":"commit","session":"12-34","message":"Fix it\n\nWhy"}"#,
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
            "approvals": [{"id": 1, "summary": {"action": "Run a shell command", "files": [],
                "scope": "This request only, once"}}],
        });
        let catalog = json!({
            "modes": ["ask"], "current_mode": "ask",
            "models": [{"engine": "claude", "selection": "opus", "name": "Opus", "id": "claude-opus"}],
            "current": {"engine": "claude", "selection": "opus"},
        });
        let recent = json!({"engine": "codex", "workspace": "/w", "session": "s", "title": "t", "started": 5});
        let data = json!({"text": "hi"});
        let changes = ChangesInfo {
            branch: Some("main".into()),
            upstream: None,
            ahead: 1,
            behind: 0,
            files: vec![FileChange {
                path: "src/a.rs".into(),
                add: 3,
                del: 1,
                binary: false,
                status: FileStatus::Modified,
            }],
            more: 2,
        };
        let diff = DiffInfo {
            path: "src/a.rs".into(),
            text: "@@ -1 +1 @@\n-a\n+b\n".into(),
            clipped: false,
            binary: false,
        };
        for (frame, direction) in [
            (changes_frame(5, &changes), Direction::ToPhone),
            (diff_frame(6, &diff), Direction::ToPhone),
            (welcome("Gemini 10"), Direction::ToPhone),
            (paired("tok", "Gemini 10"), Direction::ToPhone),
            (sessions_frame(&[session]), Direction::ToPhone),
            (
                subscribed_frame("1-2", false, &["session-1.jsonl".into()]),
                Direction::ToPhone,
            ),
            (
                event_frame("1-2", "session-1.jsonl", 3, "text", &data),
                Direction::ToPhone,
            ),
            (state_frame("1-2", &state), Direction::ToPhone),
            (catalog_frame("1-2", &catalog), Direction::ToPhone),
            (recent_frame(4, &[recent]), Direction::ToPhone),
            (ack(1, "Sent"), Direction::ToPhone),
            (
                refused(1, Code::TurnOpen, "A turn is running"),
                Direction::ToPhone,
            ),
            (bridge_state_frame(&state), Direction::FromBridge),
            (bridge_catalog_frame(&catalog), Direction::FromBridge),
            (ack(1, "Sent"), Direction::FromBridge),
            (refused(1, Code::Busy, "no"), Direction::FromBridge),
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
    fn a_refusal_can_name_the_request() {
        assert_eq!(
            req_of(r#"{"v":1,"req":5,"type":"prompt","session":"s"}"#),
            5
        );
        assert_eq!(req_of(r#"{"v":9,"req":6,"type":"launch"}"#), 6);
        for unusable in [
            "not json",
            "[1]",
            r#"{"req":"x"}"#,
            r#"{"v":1}"#,
            r#"{"req":-1}"#,
            "",
        ] {
            assert_eq!(req_of(unusable), 0, "{unusable}");
        }
        let big = format!(r#"{{"req":5,"text":"{}"}}"#, "x".repeat(MAX_MESSAGE));
        assert_eq!(req_of(&big), 0);
    }

    #[test]
    fn full_access_needs_the_exact_confirmation() {
        assert!(full_access_confirmed("ask", None));
        assert!(full_access_confirmed("auto", Some("anything")));
        assert!(!full_access_confirmed("full-access", None));
        assert!(!full_access_confirmed("full-access", Some("Full Access")));
        assert!(!full_access_confirmed("full-access", Some("full access ")));
        assert!(full_access_confirmed(
            "full-access",
            Some(FULL_ACCESS_CONFIRM)
        ));
    }

    #[test]
    fn every_named_shape_exists() {
        let named = |fields: &'static [Field]| {
            fields.iter().filter_map(|f| match f.ty {
                Ty::Object(n) | Ty::ObjectList(n) => Some(n),
                _ => None,
            })
        };
        let all = MESSAGES
            .iter()
            .map(|m| m.fields)
            .chain(OBJECTS.iter().map(|o| o.fields));
        for name in all.flat_map(named) {
            assert!(OBJECTS.iter().any(|o| o.name == name), "no shape {name}");
        }
    }

    #[test]
    fn echoed_names_are_clipped() {
        let long = "z".repeat(500);
        let kind = parse_phone(&format!(r#"{{"v":1,"req":1,"type":"{long}"}}"#)).unwrap_err();
        let field = parse_phone(&format!(
            r#"{{"v":1,"req":1,"type":"hello","token":"t","{long}":1}}"#
        ))
        .unwrap_err();
        for error in [kind, field] {
            assert!(error.len() < 120, "{error}");
            assert!(error.contains(&"z".repeat(60)), "{error}");
        }
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
        assert!(ts.contains("after?: Cursor | null;"));
        assert!(ts.contains("approvals: ApprovalInfo[];"));
        assert!(ts.contains("approval: number;"));
        assert!(ts.contains("type: \"set_mode\";"));
    }

    #[test]
    fn the_typescript_types_events_closed_sets_and_nulls() {
        let ts = typescript();
        for wanted in [
            "export type Engine = \"claude\" | \"codex\" | \"demo\" | \"opencode\";",
            "export type Mode = \"ask\" | \"accept-edits\" | \"auto\" | \"full-access\";",
            "export type Phase = \"connecting\" | \"idle\" | \"running\" | \"stopped\";",
            "export type ToolPhase = \"running\" | \"done\" | \"failed\";",
            "export type KnownJournalEvent =",
            "  | { type: \"text\"; data: string }",
            "  | { type: \"tool\"; data: ToolData }",
            "  | { type: \"approval\"; data: ApprovalData }",
            "  | { type: \"approval_closed\"; data: number }",
            "  | { type: \"started\"; data: null }",
            "  | { type: \"hook\"; data: HookData }",
            "  | { type: \"session\"; data: Record<string, unknown> }",
            "  | { type: \"models\"; data: unknown[] }",
            "export interface UnknownJournalEvent {",
            "export type JournalEvent = KnownJournalEvent | UnknownJournalEvent;",
            "export function isKnownEvent(e: JournalEvent): e is KnownJournalEvent {",
            "export interface ApprovalSummary {",
            "  summary: ApprovalSummary;",
            "  phase: ToolPhase;",
            "  decision?: string | null;",
            "  engine: Engine;",
            "  mode: Mode;",
            "  phase: Phase;",
            "  modes: Mode[];",
            "  model?: string | null;",
            "  now?: string | null;",
            "  tmux?: string | null;",
            "  code?: RefusalCode | null;",
        ] {
            assert!(ts.contains(wanted), "no {wanted:?} in\n{ts}");
        }
        // The recent frame names the request it answers.
        let recent = ts.split("export interface RecentFrame {").nth(1).unwrap();
        assert!(recent.split('}').next().unwrap().contains("req: number;"));
    }

    #[test]
    fn every_journal_record_kind_has_a_typed_shape() {
        use octet_engine::live::{
            ApprovalSummary, Event, HookOutcome, ModelInfo, Outcome, SkillInfo, ToolCall, ToolPhase,
        };
        let events = [
            Event::Ready {
                session: "s".into(),
            },
            Event::Models(vec![ModelInfo {
                selection: "opus".into(),
                id: None,
                name: "Opus".into(),
                description: String::new(),
            }]),
            Event::ModelSelected("opus".into()),
            Event::Effort(Some("high".into())),
            Event::Effort(None),
            Event::ModeChanged(octet_engine::live::Mode::Auto),
            Event::User("hi".into()),
            Event::Started,
            Event::Text("hello".into()),
            Event::Tool(ToolCall {
                id: "t1".into(),
                name: "Bash".into(),
                target: "ls".into(),
                phase: ToolPhase::Done,
                detail: "ok".into(),
            }),
            Event::Approval {
                id: 3,
                detail: "d".into(),
                summary: ApprovalSummary::new("Run a shell command"),
            },
            Event::ApprovalClosed(3),
            Event::Usage("1k tokens".into()),
            Event::Finished {
                outcome: Outcome::Completed,
            },
            Event::Skills(vec![SkillInfo {
                name: "x".into(),
                description: String::new(),
                aliases: Vec::new(),
                argument_hint: String::new(),
                path: None,
            }]),
            Event::Hook {
                name: "PreToolUse:Bash".into(),
                event: "PreToolUse".into(),
                outcome: HookOutcome::Decided("allow".into()),
                detail: String::new(),
            },
            Event::Notice("n".into()),
            Event::Error("e".into()),
            Event::Stopped,
        ];
        let mut seen = vec!["session"];
        for event in &events {
            let record = crate::event_json(event);
            let kind = record["type"].as_str().unwrap();
            let shape = EVENTS
                .iter()
                .find(|e| e.kind == kind)
                .unwrap_or_else(|| panic!("no shape for {kind}"));
            check_value(&record["data"], shape.data, kind).unwrap_or_else(|e| panic!("{e}"));
            seen.push(shape.kind);
        }
        for shape in EVENTS {
            assert!(
                seen.contains(&shape.kind),
                "{} is never recorded",
                shape.kind
            );
        }
    }

    #[test]
    fn the_closed_sets_match_the_engines_modes_and_refusal_codes() {
        let values = |name: &str| CHOICES.iter().find(|c| c.name == name).unwrap().values;
        let engines: Vec<&str> = octet_engine::live::Engine::ALL
            .iter()
            .map(|e| e.as_str())
            .collect();
        let mut sorted = engines.clone();
        sorted.sort_unstable();
        assert_eq!(values("Engine"), sorted);
        let modes: Vec<&str> = octet_engine::live::Mode::ALL
            .iter()
            .map(|m| m.label())
            .collect();
        assert_eq!(values("Mode"), modes);
        let codes: Vec<&str> = Code::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(values("RefusalCode"), codes);
        let names: Vec<&str> = CommandName::ALL.iter().map(|n| n.as_str()).collect();
        assert_eq!(values("CommandName"), names);
        for code in Code::ALL {
            assert_eq!(Code::parse(code.as_str()), Some(code));
        }
        assert_eq!(Code::parse("nope"), None);
        let every_choice = |fields: &'static [Field]| {
            fields.iter().filter_map(|f| match f.ty {
                Ty::Choice(n) | Ty::ChoiceList(n) => Some(n),
                _ => None,
            })
        };
        let all = MESSAGES
            .iter()
            .map(|m| m.fields)
            .chain(OBJECTS.iter().map(|o| o.fields));
        for name in all.flat_map(every_choice) {
            assert!(CHOICES.iter().any(|c| c.name == name), "no choice {name}");
        }
    }

    #[test]
    fn a_command_is_one_of_four_names_and_nothing_else() {
        for (name, wanted) in [
            ("new", CommandName::New),
            ("session", CommandName::Session),
            ("help", CommandName::Help),
            ("export", CommandName::Export),
        ] {
            let text =
                format!(r#"{{"v":1,"req":3,"type":"command","session":"s","name":"{name}"}}"#);
            let (req, command) = parse_phone(&text).unwrap();
            assert_eq!(req, 3);
            assert_eq!(
                command,
                PhoneCommand::Command {
                    session: "s".into(),
                    name: wanted
                }
            );
            assert_eq!(wanted.as_str(), name);
            let (_, by, bridge) = parse_bridge_command(
                &bridge_command_json(9, "iPhone", &BridgeCommand::Command { name: wanted })
                    .to_string(),
            )
            .unwrap();
            assert_eq!(
                (by.as_str(), bridge),
                ("iPhone", BridgeCommand::Command { name: wanted })
            );
        }
        for name in [
            "model",
            "New",
            "/new",
            "export /tmp/x",
            "export ",
            "",
            "quit",
        ] {
            let text = format!(
                r#"{{"v":1,"req":3,"type":"command","session":"s","name":{}}}"#,
                json!(name)
            );
            assert!(parse_phone(&text).is_err(), "{name:?} was taken");
        }
        // A path, an argument or any other field is refused, as is a name that is not a string.
        for text in [
            r#"{"v":1,"req":3,"type":"command","session":"s","name":"export","path":"/tmp/x"}"#,
            r#"{"v":1,"req":3,"type":"command","session":"s","name":"export","argument":""}"#,
            r#"{"v":1,"req":3,"type":"command","session":"s","name":7}"#,
            r#"{"v":1,"req":3,"type":"command","session":"s"}"#,
            r#"{"v":1,"req":3,"type":"command","name":"help"}"#,
        ] {
            assert!(parse_phone(text).is_err(), "{text}");
        }
        assert_eq!(CommandName::parse("help"), Some(CommandName::Help));
        assert_eq!(CommandName::parse("nope"), None);
    }

    #[test]
    fn the_command_kind_generates_run_command_names() {
        let named = |direction: Direction| {
            MESSAGES
                .iter()
                .find(|m| m.direction == direction && m.kind == "command")
                .map(interface_name)
        };
        assert_eq!(named(FromPhone).as_deref(), Some("RunCommand"));
        assert_eq!(named(ToBridge).as_deref(), Some("BridgeRunCommand"));
        let ts = typescript();
        assert!(!ts.contains("CommandCommand"), "a doubled name");
        assert!(ts.contains("type: \"command\";"), "the wire kind stays");
    }

    #[test]
    fn an_unknown_mode_is_refused_at_the_hub() {
        // The hub refuses it as `invalid`, in its own words, before any session sees it (the
        // terminal UI would refuse it too, but only after the hub had sent it on).
        assert_eq!(
            parse_phone(r#"{"v":1,"req":1,"type":"set_mode","session":"s","mode":"sideways"}"#),
            Err(UNKNOWN_MODE.to_owned())
        );
    }

    #[test]
    fn debug_output_never_shows_a_token_or_a_pairing_secret() {
        let secret = "f00dfeed".repeat(8);
        let hello = format!(r#"{{"v":1,"req":1,"type":"hello","token":"{secret}"}}"#);
        let pair = format!(
            r#"{{"v":1,"req":2,"type":"pair","secret":"{secret}","name":"Gemini 10","platform":"android"}}"#
        );
        for text in [hello, pair] {
            let (_, command) = parse_phone(&text).unwrap();
            let shown = format!("{command:?}");
            assert!(!shown.contains(&secret), "{shown}");
            assert!(shown.contains("redacted"), "{shown}");
        }
        let (_, command) = parse_phone(&format!(
            r#"{{"v":1,"req":2,"type":"pair","secret":"{secret}","name":"Gemini 10","platform":"android"}}"#
        ))
        .unwrap();
        assert!(
            format!("{command:?}").contains("Gemini 10"),
            "the name is not a secret"
        );
        let PhoneCommand::Pair { secret: kept, .. } = command else {
            panic!("a pair")
        };
        assert_eq!(kept.expose(), secret);
    }

    #[test]
    fn register_push_takes_an_expo_token_or_null_and_refuses_anything_else() {
        let accepted = [
            "ExpoPushToken[abc-DEF_123]",
            "ExponentPushToken[xxxxxxxxxxxxxxxxxxxxxx]",
            &format!("ExpoPushToken[{}]", "a".repeat(200)),
        ];
        for token in accepted {
            let text = format!(r#"{{"v":1,"req":4,"type":"register_push","token":"{token}"}}"#);
            let (req, command) = parse_phone(&text).unwrap_or_else(|e| panic!("{token}: {e}"));
            assert_eq!(req, 4);
            let PhoneCommand::RegisterPush {
                token: Some(kept), ..
            } = command
            else {
                panic!("a registration of {token}")
            };
            assert_eq!(kept.expose(), token);
        }
        let (_, off) =
            parse_phone(r#"{"v":1,"req":5,"type":"register_push","token":null}"#).unwrap();
        assert_eq!(
            off,
            PhoneCommand::RegisterPush {
                token: None,
                kinds: None
            }
        );
        let too_long = format!("ExpoPushToken[{}]", "a".repeat(201));
        let refused = [
            r#"{"v":1,"req":6,"type":"register_push"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":""}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"ExpoPushToken[]"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"ExpoPushToken[a b]"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"expopushtoken[abc]"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"ExpoPushToken[abc]x"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"xExpoPushToken[abc]"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"ExpoPushToken[abc]\n"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"https://evil.example/x"}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":7}"#.to_owned(),
            r#"{"v":1,"req":6,"type":"register_push","token":"ExpoPushToken[abc]","extra":1}"#
                .to_owned(),
            format!(r#"{{"v":1,"req":6,"type":"register_push","token":"{too_long}"}}"#),
        ];
        for text in &refused {
            let reason = parse_phone(text).expect_err(text);
            assert!(
                !reason.contains("abc]"),
                "the reason repeats the token: {reason}"
            );
        }
    }

    #[test]
    fn register_push_token_is_required_but_nullable_in_the_typescript() {
        let ts = typescript();
        let interface = ts
            .split("export interface RegisterPushCommand {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the interface");
        assert!(interface.contains("  token: string | null;"), "{interface}");
        // Optional fields stay optional, so only this one changed.
        assert!(ts.contains("  after?: Cursor | null;"));
        // The parser takes what the TypeScript allows and refuses what it forbids.
        assert!(parse_phone(r#"{"v":1,"req":1,"type":"register_push","token":null}"#).is_ok());
        assert!(parse_phone(r#"{"v":1,"req":1,"type":"register_push"}"#).is_err());
    }

    #[test]
    fn register_push_takes_optional_kinds_all_three_said() {
        let with = |kinds: &str| {
            format!(r#"{{"v":1,"req":2,"type":"register_push","token":null,"kinds":{kinds}}}"#)
        };
        let (_, command) =
            parse_phone(&with(r#"{"approvals":true,"errors":false,"turns":true}"#)).unwrap();
        assert_eq!(
            command,
            PhoneCommand::RegisterPush {
                token: None,
                kinds: Some(PushKinds {
                    approvals: true,
                    errors: false,
                    turns: true
                }),
            }
        );
        // Left out, or null: the hub keeps what it has.
        for text in [
            r#"{"v":1,"req":2,"type":"register_push","token":null}"#.to_owned(),
            with("null"),
        ] {
            let (_, command) = parse_phone(&text).unwrap();
            assert_eq!(
                command,
                PhoneCommand::RegisterPush {
                    token: None,
                    kinds: None
                }
            );
        }
        for kinds in [
            r#"{"approvals":true,"errors":true}"#,
            r#"{"approvals":true,"errors":true,"turns":1}"#,
            r#"{"approvals":true,"errors":true,"turns":false,"sound":true}"#,
            r#""all""#,
            "[]",
        ] {
            assert!(parse_phone(&with(kinds)).is_err(), "{kinds}");
        }
        // New devices hear of approvals and errors, not of every finished turn.
        assert_eq!(
            PushKinds::default(),
            PushKinds {
                approvals: true,
                errors: true,
                turns: false
            }
        );
        let ts = typescript();
        assert!(ts.contains("  kinds?: PushKinds | null;"), "{ts}");
        assert!(ts.contains("export interface PushKinds {"));
    }

    #[test]
    fn a_sessions_row_may_say_its_usage_and_that_it_asked() {
        let ts = typescript();
        let row = ts
            .split("export interface SessionInfo {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the interface");
        assert!(row.contains("  usage?: string | null;"), "{row}");
        assert!(row.contains("  question?: boolean | null;"), "{row}");
        let frame = sessions_frame(&[json!({
            "id": "1-2", "engine": "claude", "workspace": "/w", "mode": "ask", "phase": "idle",
            "approvals": 0, "from_phone": false, "usage": "5h 12%", "question": true,
        })]);
        let message = MESSAGES.iter().find(|m| m.kind == "sessions").unwrap();
        check_fields(
            frame.as_object().unwrap(),
            message.fields,
            &["v", "type"],
            "",
        )
        .unwrap_or_else(|e| panic!("{frame}: {e}"));
        let asked = |question: Value| {
            let mut frame = frame.clone();
            frame["sessions"][0]["question"] = question;
            check_fields(
                frame.as_object().unwrap(),
                message.fields,
                &["v", "type"],
                "",
            )
        };
        assert!(asked(json!("yes")).is_err());
    }

    #[test]
    fn ping_parses_and_takes_no_fields() {
        let (req, command) = parse_phone(r#"{"v":1,"req":9,"type":"ping"}"#).unwrap();
        assert_eq!((req, command), (9, PhoneCommand::Ping));
        assert!(parse_phone(r#"{"v":1,"req":9,"type":"ping","x":1}"#).is_err());
    }

    #[test]
    fn a_push_registration_never_shows_its_token_in_debug_output() {
        let token = "ExpoPushToken[Zk3_secretvalue-77]";
        let text = format!(r#"{{"v":1,"req":1,"type":"register_push","token":"{token}"}}"#);
        let (_, command) = parse_phone(&text).unwrap();
        let shown = format!("{command:?}");
        assert!(!shown.contains("secretvalue"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
        let off = PhoneCommand::RegisterPush {
            token: None,
            kinds: None,
        };
        assert!(!format!("{off:?}").contains("redacted"));
    }

    #[test]
    fn set_mode_names_one_of_the_modes() {
        let set = |mode: &str| {
            parse_phone(&format!(
                r#"{{"v":1,"req":1,"type":"set_mode","session":"12-34","mode":{}}}"#,
                json!(mode)
            ))
        };
        for mode in ["ask", "accept-edits", "auto", "full-access"] {
            assert!(set(mode).is_ok(), "{mode}");
        }
        // Only the labels the terminal UI takes: no vendor names, no other case, no spaces.
        for mode in [
            "",
            "Ask",
            "ASK",
            " ask",
            "ask ",
            "bypassPermissions",
            "plan",
            "acceptEdits",
            "full access",
            "full_access",
        ] {
            let refused = set(mode).expect_err(mode);
            assert!(refused.contains("mode"), "{mode:?}: {refused}");
            assert!(!refused.contains("bypassPermissions"), "{refused}");
        }
    }

    #[test]
    fn hello_may_say_it_is_a_background_connection() {
        let hello = |extra: &str| {
            parse_phone(&format!(
                r#"{{"v":1,"req":1,"type":"hello","token":"t"{extra}}}"#
            ))
        };
        let background = |extra: &str| match hello(extra) {
            Ok((_, PhoneCommand::Hello { background, .. })) => Ok(background),
            other => Err(format!("{other:?}")),
        };
        // An old app's hello, without the field, is the app in front.
        assert_eq!(background(""), Ok(false));
        assert_eq!(background(r#","background":false"#), Ok(false));
        assert_eq!(background(r#","background":true"#), Ok(true));
        // `null` is left out, as for every optional field.
        assert_eq!(background(r#","background":null"#), Ok(false));
        // Only a boolean.
        for bad in [
            r#","background":"yes""#,
            r#","background":1"#,
            r#","background":[]"#,
        ] {
            let refused = hello(bad).expect_err(bad);
            assert!(refused.contains("background"), "{bad}: {refused}");
        }
    }

    #[test]
    fn register_activity_takes_lowercase_hex_tokens_and_refuses_anything_else() {
        // iOS 26.5 issues 128-byte tokens: 256 hex digits. Any up to 1024 is taken.
        let ios = "0123456789abcdef".repeat(16);
        let long = "a".repeat(1024);
        // Either token may be left out (or null), as long as one is there.
        let accepted: [(String, Option<&str>, Option<&str>); 8] = [
            (
                r#""token":"0123456789abcdef""#.to_owned(),
                Some("0123456789abcdef"),
                None,
            ),
            (
                r#""token":"ab","start_token":"cd""#.to_owned(),
                Some("ab"),
                Some("cd"),
            ),
            (
                r#""token":"7","start_token":null"#.to_owned(),
                Some("7"),
                None,
            ),
            (r#""start_token":"cd""#.to_owned(), None, Some("cd")),
            (
                r#""token":null,"start_token":"cd""#.to_owned(),
                None,
                Some("cd"),
            ),
            (
                r#""token":"ab","start_token":"ab""#.to_owned(),
                Some("ab"),
                Some("ab"),
            ),
            (
                format!(r#""token":"{ios}","start_token":"{ios}""#),
                Some(ios.as_str()),
                Some(ios.as_str()),
            ),
            (
                format!(r#""token":"{long}","start_token":"{long}""#),
                Some(long.as_str()),
                Some(long.as_str()),
            ),
        ];
        for (fields, token, start) in &accepted {
            let text = format!(r#"{{"v":1,"req":4,"type":"register_activity",{fields}}}"#);
            let (req, command) = parse_phone(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(req, 4);
            let PhoneCommand::RegisterActivity {
                token: kept,
                start_token: kept_start,
            } = command
            else {
                panic!("a registration of {text}")
            };
            assert_eq!(kept.as_ref().map(Secret::expose), *token);
            assert_eq!(kept_start.as_ref().map(Secret::expose), *start);
        }
        let too_long = "a".repeat(1025);
        let refused = [
            String::new(),
            r#","token":null"#.to_owned(),
            r#","start_token":null"#.to_owned(),
            r#","token":null,"start_token":null"#.to_owned(),
            r#","token":"""#.to_owned(),
            r#","token":"ABC""#.to_owned(),
            r#","token":"abC""#.to_owned(),
            r#","token":"abg""#.to_owned(),
            r#","token":"ab cd""#.to_owned(),
            r#","token":"abc\n""#.to_owned(),
            r#","token":"0xabc""#.to_owned(),
            r#","token":7"#.to_owned(),
            format!(r#","token":"{too_long}""#),
            r#","start_token":"""#.to_owned(),
            r#","start_token":"ABC""#.to_owned(),
            r#","start_token":"xyz""#.to_owned(),
            r#","start_token":7"#.to_owned(),
            format!(r#","start_token":"{too_long}""#),
            r#","token":"abc","start_token":"""#.to_owned(),
            r#","token":"abc","start_token":"ABC""#.to_owned(),
            r#","token":"ABC","start_token":"abc""#.to_owned(),
            r#","token":"abc","extra":1"#.to_owned(),
            r#","start_token":"abc","kinds":null"#.to_owned(),
        ];
        for fields in &refused {
            let text = format!(r#"{{"v":1,"req":6,"type":"register_activity"{fields}}}"#);
            let reason = parse_phone(&text).expect_err(&text);
            assert!(
                !reason.contains("abc") && !reason.contains("ABC") && !reason.contains("xyz"),
                "the reason repeats the token: {reason}"
            );
        }
        assert!(is_activity_token("0123456789abcdef"));
        assert!(!is_activity_token(""));
        assert!(!is_activity_token("A"));
        assert_eq!(ios.len(), 256);
        assert!(is_activity_token(&ios));
        assert!(is_activity_token(&long));
        assert!(!is_activity_token(&too_long));
    }

    #[test]
    fn clear_activity_takes_no_fields() {
        let (req, command) = parse_phone(r#"{"v":1,"req":3,"type":"clear_activity"}"#).unwrap();
        assert_eq!((req, command), (3, PhoneCommand::ClearActivity));
        for fields in [r#","token":"abc""#, r#","x":1"#, r#","start_token":null"#] {
            let text = format!(r#"{{"v":1,"req":3,"type":"clear_activity"{fields}}}"#);
            let reason = parse_phone(&text).expect_err(&text);
            assert!(!reason.contains("abc"), "{reason}");
        }
        let ts = typescript();
        let interface = ts
            .split("export interface ClearActivityCommand {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the interface");
        assert!(
            interface.contains("  type: \"clear_activity\";"),
            "{interface}"
        );
        assert!(ts.contains("  | ClearActivityCommand"), "{ts}");
    }

    #[test]
    fn an_activity_registration_never_shows_its_tokens_in_debug_output() {
        let text = r#"{"v":1,"req":1,"type":"register_activity","token":"5ec7e7aaaa","start_token":"5ec7e7bbbb"}"#;
        let (_, command) = parse_phone(text).unwrap();
        let shown = format!("{command:?}");
        assert!(!shown.contains("5ec7e7"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn register_activity_has_two_optional_tokens_in_the_typescript() {
        let ts = typescript();
        let interface = ts
            .split("export interface RegisterActivityCommand {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("the interface");
        assert!(
            interface.contains("  type: \"register_activity\";"),
            "{interface}"
        );
        assert!(
            interface.contains("  token?: string | null;"),
            "{interface}"
        );
        assert!(
            interface.contains("  start_token?: string | null;"),
            "{interface}"
        );
    }

    #[test]
    fn the_known_journal_kinds_are_listed_for_the_guard() {
        let ts = typescript();
        let list = ts
            .split("export const KNOWN_JOURNAL_KINDS: readonly string[] = [")
            .nth(1)
            .expect("the kinds array")
            .split("];")
            .next()
            .unwrap();
        let listed: Vec<&str> = list
            .split(',')
            .map(|k| k.trim().trim_matches('"'))
            .filter(|k| !k.is_empty())
            .collect();
        let kinds: Vec<&str> = EVENTS.iter().map(|e| e.kind).collect();
        assert_eq!(listed, kinds);
        assert!(ts.contains("return KNOWN_JOURNAL_KINDS.includes(e.type);"));
        // The catch-all is its own type, so it cannot absorb the known discriminants.
        assert!(!ts.contains("  | { type: string; data: unknown }"));
        let unknown = ts
            .split("export interface UnknownJournalEvent {")
            .nth(1)
            .unwrap();
        let unknown = unknown.split('}').next().unwrap();
        assert!(unknown.contains("type: string;") && unknown.contains("data: unknown;"));
    }

    #[test]
    fn git_commands_carry_their_fields() {
        let (_, changes) =
            parse_phone(r#"{"v":1,"req":1,"type":"changes","session":"s"}"#).unwrap();
        assert_eq!(
            changes,
            PhoneCommand::Changes {
                session: "s".into()
            }
        );
        let (_, diff) =
            parse_phone(r#"{"v":1,"req":1,"type":"diff","session":"s","path":"-rf \"x\"\n"}"#)
                .unwrap();
        assert_eq!(
            diff,
            PhoneCommand::Diff {
                session: "s".into(),
                path: "-rf \"x\"\n".into()
            }
        );
        let (_, commit) =
            parse_phone(r#"{"v":1,"req":1,"type":"commit","session":"s","message":"One line"}"#)
                .unwrap();
        assert_eq!(
            commit,
            PhoneCommand::Commit {
                session: "s".into(),
                message: "One line".into()
            }
        );
        let longest = json!({"v": 1, "req": 1, "type": "diff", "session": "s",
            "path": "é".repeat(PATH_LIMIT)});
        assert!(parse_phone(&longest.to_string()).is_ok());
        let longest = json!({"v": 1, "req": 1, "type": "commit", "session": "s",
            "message": "é".repeat(COMMIT_MESSAGE_LIMIT)});
        assert!(parse_phone(&longest.to_string()).is_ok());
    }

    #[test]
    fn git_commands_refuse_extra_fields_a_nul_and_a_long_path() {
        let refusals = [
            (
                json!({"v": 1, "req": 1, "type": "changes", "session": "s", "ref": "HEAD~3"}),
                "Unexpected field ref",
            ),
            (
                json!({"v": 1, "req": 1, "type": "diff", "session": "s", "path": "a",
                    "args": ["--output=/tmp/x"]}),
                "Unexpected field args",
            ),
            (
                json!({"v": 1, "req": 1, "type": "commit", "session": "s", "message": "m",
                    "amend": true}),
                "Unexpected field amend",
            ),
            (
                json!({"v": 1, "req": 1, "type": "diff", "session": "s",
                    "path": "a".repeat(PATH_LIMIT + 1)}),
                PATH_TOO_LONG,
            ),
            (
                json!({"v": 1, "req": 1, "type": "diff", "session": "s", "path": "a\u{0}b"}),
                "path",
            ),
            (
                json!({"v": 1, "req": 1, "type": "diff", "session": "s", "path": ""}),
                "path",
            ),
            (
                json!({"v": 1, "req": 1, "type": "commit", "session": "s",
                    "message": "fine\u{0}--amend"}),
                NO_COMMIT_MESSAGE,
            ),
            (
                json!({"v": 1, "req": 1, "type": "commit", "session": "s", "message": ""}),
                NO_COMMIT_MESSAGE,
            ),
            (
                json!({"v": 1, "req": 1, "type": "commit", "session": "s", "message": " \n\t"}),
                NO_COMMIT_MESSAGE,
            ),
            (
                json!({"v": 1, "req": 1, "type": "commit", "session": "s",
                    "message": "m".repeat(COMMIT_MESSAGE_LIMIT + 1)}),
                NO_COMMIT_MESSAGE,
            ),
            (
                json!({"v": 1, "req": 1, "type": "commit", "session": "s"}),
                "Missing message",
            ),
            (
                json!({"v": 1, "req": 1, "type": "diff", "path": "a"}),
                "Missing session",
            ),
        ];
        for (command, wanted) in refusals {
            let error = parse_phone(&command.to_string()).unwrap_err();
            assert!(error.contains(wanted), "{command}: {error}");
            // A refusal never echoes the message or the path back.
            assert!(!error.contains("--amend") && error.len() < 120, "{error}");
        }
    }

    #[test]
    fn the_typescript_types_changes_and_diffs() {
        let ts = typescript();
        for wanted in [
            "export type FileStatus = \"M\" | \"A\" | \"D\" | \"R\" | \"?\" | \"U\";",
            "export interface FileChange {",
            "  status: FileStatus;",
            "export interface ChangesFrame {",
            "  files: FileChange[];",
            "  branch?: string | null;",
            "export interface DiffFrame {",
            "  clipped: boolean;",
            "export interface ChangesCommand {",
            "export interface DiffCommand {",
            "export interface CommitCommand {",
        ] {
            assert!(ts.contains(wanted), "no {wanted:?}");
        }
        for status in FileStatus::ALL {
            assert_eq!(FileStatus::parse(status.as_str()), Some(status));
        }
        let spelled: Vec<&str> = FileStatus::ALL.iter().map(|s| s.as_str()).collect();
        let set = CHOICES.iter().find(|c| c.name == "FileStatus").unwrap();
        assert_eq!(set.values, spelled);
        assert_eq!(FileStatus::parse("X"), None);
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
