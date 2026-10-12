//! The conversation as wrapped, coloured lines, cached per entry and width.
use super::{ACCENT, AMBER, FG, MUTED};
use crate::app::{App, Role};
use ratatui::prelude::*;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl App {
    pub(crate) fn visible_lines(&mut self, width: u16, height: usize) -> Vec<Line<'static>> {
        if let Some(entries) = self.chat.catalog_focus.take() {
            let mut rows = 0usize;
            for entry in self.chat.entries.iter_mut().rev().take(entries) {
                rows += entry.rows(width).len();
            }
            self.chat.scroll = rows.saturating_sub(height);
        }
        let needed = self.chat.scroll.saturating_add(height);
        // First bring the caches up to date as far back as the view reaches,
        // then borrow rows from them and clone only those on screen.
        let mut available = 0usize;
        for entry in self.chat.entries.iter_mut().rev() {
            available += entry.rows(width).len();
            if available >= needed {
                break;
            }
        }
        let lines: Vec<&Line<'static>> = self
            .chat
            .entries
            .iter()
            .rev()
            .flat_map(|entry| entry.cache.iter().rev())
            .take(needed)
            .collect();
        // History shorter than the view needs: scroll no further than a
        // full page from its start, rather than showing emptiness below.
        if lines.len() < needed {
            self.chat.scroll = self.chat.scroll.min(lines.len().saturating_sub(height));
        }
        lines
            .into_iter()
            .skip(self.chat.scroll)
            .take(height)
            .rev()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
thread_local! {
    /// Bytes of entry text wrapped on this thread, for tests.
    pub(crate) static WRAPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// Where an entry's last source line starts, so text appended to the entry
/// re-wraps from there rather than from the top.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Tail {
    /// The line's byte offset in the entry's text.
    start: usize,
    /// The rows it wrapped to, at the end of the cache.
    rows: usize,
    /// Whether it starts inside a code fence.
    code: bool,
    /// Text was appended since the rows were wrapped.
    pub(crate) grown: bool,
}
impl crate::app::Entry {
    /// Its rows at `width`, wrapped only as far as they changed: appended
    /// text re-wraps the last source line onwards.
    pub(crate) fn rows(&mut self, width: u16) -> &[Line<'static>] {
        if self.role == Role::Tool {
            if self.width != Some(width) {
                self.cache = tool_rows(&self.tools, self.expanded, self.turn_over, width);
                self.width = Some(width);
            }
            return &self.cache;
        }
        if self.width != Some(width) {
            let (lines, tail) = wrapped(self.role, &self.text, width);
            self.cache = lines;
            self.tail = tail;
            self.width = Some(width);
        } else if self.tail.grown {
            self.cache.truncate(self.cache.len() - self.tail.rows);
            let Tail { start, code, .. } = self.tail;
            let (lines, tail) = body(self.role, &self.text[start..], width, code, start);
            self.cache.extend(lines);
            self.tail = tail;
        }
        &self.cache
    }
}
/// Rows of a failed call kept when collapsed.
const FAILED_ROWS: usize = 8;
/// A tool's name as the transcript says it.
pub(crate) fn label(name: &str) -> &str {
    match name {
        "commandExecution" => "Bash",
        "fileChange" => "Edit",
        "mcpToolCall" => "MCP",
        other => other,
    }
}
/// The finished calls in words, by name in order of first appearance: "Read 3 files · Ran 1 command".
pub(crate) fn summary(calls: &[octet_core::ToolCall]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        // Calls that read alike in words are counted together.
        let name = match label(&call.name) {
            "MultiEdit" | "NotebookEdit" => "Edit",
            "Glob" => "Grep",
            other => other,
        };
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => counts.push((name, 1)),
        }
    }
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    counts
        .into_iter()
        .map(|(name, n)| match name {
            "Read" => format!("Read {}", plural(n, "file", "files")),
            "Edit" => format!("Edited {}", plural(n, "file", "files")),
            "Write" => format!("Wrote {}", plural(n, "file", "files")),
            "Bash" => format!("Ran {}", plural(n, "command", "commands")),
            "Grep" if n == 1 => "Searched once".to_owned(),
            "Grep" => format!("Searched {n} times"),
            other => format!("{other} ×{n}"),
        })
        .collect::<Vec<_>>()
        .join(" · ")
}
/// Appends `text` wrapped to `width` in `style`.
fn push(rows: &mut Vec<Line<'static>>, text: &str, style: Style, width: usize) {
    for row in wrap(text, width) {
        rows.push(Line::from(Span::styled(row, style)));
    }
}
/// A call's first row: its mark, name and target, and "stopped" when its turn ended mid-call.
fn head(call: &octet_core::ToolCall, mark: &str, turn_over: bool) -> String {
    let stopped = if call.phase == octet_core::ToolPhase::Running && turn_over {
        " · stopped"
    } else {
        ""
    };
    format!("{mark} {} {}{stopped}", label(&call.name), call.target)
        .trim_end()
        .to_owned()
}
/// A call's detail lines, indented; with `keep`, only the last that many and a count of the rest.
fn detail(rows: &mut Vec<Line<'static>>, text: &str, keep: Option<usize>, width: usize) {
    let lines: Vec<&str> = text.lines().collect();
    let skip = keep.map_or(0, |keep| lines.len().saturating_sub(keep));
    let style = Style::default().fg(MUTED);
    for line in &lines[skip..] {
        push(rows, &format!("  {line}"), style, width);
    }
    if skip > 0 {
        push(
            rows,
            &format!("  +{skip} more lines · Ctrl+O"),
            style,
            width,
        );
    }
}
/// A tools entry's rows: collapsed, a summary of finished calls, a line per running call and
/// each failure open (cut to its last `FAILED_ROWS` lines); expanded, every call and its detail.
fn tool_rows(
    calls: &[octet_core::ToolCall],
    expanded: bool,
    turn_over: bool,
    width: u16,
) -> Vec<Line<'static>> {
    use octet_core::ToolPhase::{Done, Failed, Running};
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    if expanded {
        for call in calls {
            let (mark, color) = match call.phase {
                Done => ("✓", MUTED),
                Failed => ("✗", AMBER),
                Running => ("…", MUTED),
            };
            push(
                &mut rows,
                &head(call, mark, turn_over),
                Style::default().fg(color),
                width,
            );
            detail(&mut rows, &call.detail, None, width);
        }
        return rows;
    }
    let done: Vec<octet_core::ToolCall> =
        calls.iter().filter(|c| c.phase == Done).cloned().collect();
    if !done.is_empty() {
        let text = format!("▸ {}   Ctrl+O details", summary(&done));
        push(&mut rows, &text, Style::default().fg(MUTED), width);
    }
    for call in calls.iter().filter(|c| c.phase == Running) {
        push(
            &mut rows,
            &head(call, "…", turn_over),
            Style::default().fg(MUTED),
            width,
        );
    }
    for call in calls.iter().filter(|c| c.phase == Failed) {
        push(
            &mut rows,
            &head(call, "✗", turn_over),
            Style::default().fg(AMBER),
            width,
        );
        detail(&mut rows, &call.detail, Some(FAILED_ROWS), width);
    }
    rows
}
#[cfg(test)]
fn entry_lines(role: Role, text: &str, width: u16) -> Vec<Line<'static>> {
    wrapped(role, text, width).0
}
/// An entry's rows at `width`, and where its last source line starts.
fn wrapped(role: Role, text: &str, width: u16) -> (Vec<Line<'static>>, Tail) {
    let (label, color) = match role {
        Role::User => ("YOU", ACCENT),
        Role::Assistant => ("OCTET", FG),
        Role::Tool => ("TOOL", MUTED),
        Role::Notice => ("NOTE", MUTED),
        Role::Error => ("ERROR", AMBER),
        Role::Shell => ("SHELL", MUTED),
        Role::ShellFailed => ("SHELL", AMBER),
    };
    let mut result = vec![
        Line::default(),
        Line::from(Span::styled(label, Style::default().fg(color).bold())),
    ];
    let (lines, tail) = body(role, text, width, false, 0);
    result.extend(lines);
    (result, tail)
}
/// The rows of `text`, which starts at byte `offset` of its entry, `code`
/// saying whether it starts inside a fence; and where its last line starts.
fn body(
    role: Role,
    text: &str,
    width: u16,
    mut code: bool,
    offset: usize,
) -> (Vec<Line<'static>>, Tail) {
    #[cfg(test)]
    WRAPPED.set(WRAPPED.get() + text.len());
    let mut result = Vec::new();
    let mut tail = Tail::default();
    let mut start = offset;
    for line in text.split('\n') {
        let before = result.len();
        tail = Tail {
            start,
            rows: 0,
            code,
            grown: false,
        };
        start += line.len() + 1;
        if line.starts_with("```") {
            code = !code;
            result.push(Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(MUTED),
            )));
        } else {
            let style = if role == Role::Error {
                Style::default().fg(AMBER)
            } else if role == Role::Tool || code {
                Style::default().fg(MUTED)
            } else if line.starts_with('#') {
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(FG)
            };
            for wrapped in wrap(line, width.max(1) as usize) {
                result.push(Line::from(Span::styled(wrapped, style)));
            }
        }
        tail.rows = result.len() - before;
    }
    (result, tail)
}
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut col = 0;
    for word in text.split_word_bounds() {
        // Measured as ratatui draws it, grapheme by grapheme: the string
        // width of a word can be less (Arabic lam-alef counts as one).
        let word_width: usize = word.graphemes(true).map(UnicodeWidthStr::width).sum();
        if word_width <= width {
            if col + word_width > width && col > 0 {
                rows.push(String::new());
                col = 0;
            }
            if col == 0 && rows.len() > 1 && word.trim().is_empty() {
                continue;
            }
            rows.last_mut()
                .expect("rows starts non-empty")
                .push_str(word);
            col += word_width;
        } else {
            for g in word.graphemes(true) {
                let g_width = g.width();
                if col + g_width > width && col > 0 {
                    rows.push(String::new());
                    col = 0;
                }
                rows.last_mut().expect("rows starts non-empty").push_str(g);
                col += g_width;
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Entry;
    use octet_core::Engine;
    use std::fmt::Write as _;
    /// A row's width as ratatui draws it: the sum of its graphemes' widths.
    fn drawn_width(row: &str) -> usize {
        row.graphemes(true).map(UnicodeWidthStr::width).sum()
    }
    #[test]
    fn wrap_never_exceeds_the_width() {
        let samples = [
            "لا ".repeat(40),
            "界".repeat(50),
            "👩‍💻 ".repeat(30),
            "e\u{301} ".repeat(50),
            "mixed 界 لا 👩‍💻 words and more words".repeat(4),
        ];
        for text in &samples {
            for width in 2..30 {
                for row in wrap(text, width) {
                    let one_wide_grapheme = row.graphemes(true).count() == 1;
                    assert!(
                        drawn_width(&row) <= width || one_wide_grapheme,
                        "{row:?} is {} wide at {width}",
                        drawn_width(&row)
                    );
                }
            }
        }
    }

    /// A reply with fences, wide characters, a long word and blank lines.
    fn sample() -> String {
        let mut text =
            String::from("Intro with some words\n```rust\nfn main() { let 界 = \"😀😀\"; }\n```\n");
        text.push_str(&"a".repeat(300));
        text.push_str("\n\n# Heading\n");
        for n in 0..40 {
            let _ = writeln!(
                text,
                "line {n} wide 界界界 text that wraps across the narrow view"
            );
        }
        text.push_str("```\nunclosed fence at the end");
        text
    }

    fn streamed(text: &str, chunk: usize, widths: [u16; 2]) {
        let chars: Vec<char> = text.chars().collect();
        let mut entry = Entry::new(Role::Assistant, Engine::CLAUDE, String::new());
        for (n, piece) in chars.chunks(chunk).enumerate() {
            entry.append(&piece.iter().collect::<String>());
            // Halfway, the view changes width.
            let width = if n * chunk < chars.len() / 2 {
                widths[0]
            } else {
                widths[1]
            };
            let full = entry_lines(entry.role, &entry.text, width);
            assert_eq!(
                entry.rows(width),
                full.as_slice(),
                "chunk {chunk}, piece {n}"
            );
        }
    }

    #[test]
    fn streamed_text_wraps_as_a_full_rewrap_does() {
        let text = sample();
        for chunk in [1, 2, 5, 13, 64, 500] {
            streamed(&text, chunk, [7, 7]);
            streamed(&text, chunk, [40, 23]);
        }
    }

    #[test]
    fn a_dropped_start_rewraps_at_any_width() {
        for width in [0, 40] {
            let mut entry =
                Entry::new(Role::Assistant, Engine::CLAUDE, "first line\nsecond".into());
            let _ = entry.rows(width);
            entry.append(" line");
            entry.text.drain(..6);
            entry.invalidate();
            let full = entry_lines(entry.role, &entry.text, width);
            assert_eq!(entry.rows(width), full.as_slice(), "width {width}");
        }
    }

    #[test]
    fn appending_rewraps_only_the_last_line() {
        let mut entry = Entry::new(
            Role::Assistant,
            Engine::CLAUDE,
            "word ".repeat(12_000).replace("word word ", "word\nword "),
        );
        let _ = entry.rows(80);
        WRAPPED.set(0);
        entry.append("more words");
        let _ = entry.rows(80);
        assert!(WRAPPED.get() < 1024, "re-wrapped {} bytes", WRAPPED.get());
    }

    fn call(
        id: &str,
        name: &str,
        target: &str,
        phase: octet_core::ToolPhase,
        detail: &str,
    ) -> octet_core::ToolCall {
        octet_core::ToolCall {
            id: id.into(),
            name: name.into(),
            target: target.into(),
            phase,
            detail: detail.into(),
        }
    }
    fn texts(rows: &[Line<'static>]) -> Vec<String> {
        rows.iter().map(ToString::to_string).collect()
    }
    #[test]
    fn collapsed_calls_summarise_done_show_running_and_open_failures() {
        use octet_core::ToolPhase::*;
        let mut entry = Entry::tools(Engine::CLAUDE, false);
        for c in [
            call("1", "Read", "a.rs", Done, "x"),
            call("2", "Read", "b.rs", Done, "x"),
            call("3", "Read", "c.rs", Done, "x"),
            call("4", "commandExecution", "cargo build", Done, "x"),
            call("5", "Grep", "fn main", Running, "x"),
            call(
                "6",
                "Bash",
                "cargo test",
                Failed,
                &(1..=12)
                    .map(|n| format!("out {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ] {
            entry.record(c);
        }
        let rows = texts(entry.rows(80));
        assert_eq!(rows[0], "▸ Read 3 files · Ran 1 command   Ctrl+O details");
        assert_eq!(rows[1], "… Grep fn main");
        assert_eq!(rows[2], "✗ Bash cargo test");
        assert_eq!(
            &rows[3..11],
            (5..=12)
                .map(|n| format!("  out {n}"))
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert_eq!(rows[11], "  +4 more lines · Ctrl+O");
        assert_eq!(rows.len(), 12, "no label row, no blank line");
    }
    #[test]
    fn expanded_calls_list_each_with_its_detail() {
        use octet_core::ToolPhase::*;
        let mut entry = Entry::tools(Engine::CLAUDE, true);
        entry.record(call("1", "Read", "a.rs", Done, "line one\nline two"));
        entry.record(call("2", "Bash", "ls", Failed, "boom"));
        assert_eq!(
            texts(entry.rows(80)),
            [
                "✓ Read a.rs",
                "  line one",
                "  line two",
                "✗ Bash ls",
                "  boom"
            ]
        );
    }
    #[test]
    fn summary_words_are_singular_for_one() {
        assert_eq!(
            summary(&[call("1", "Read", "a", octet_core::ToolPhase::Done, "")]),
            "Read 1 file"
        );
        let grep = call("1", "Grep", "a", octet_core::ToolPhase::Done, "");
        let fetch = call("2", "WebFetch", "u", octet_core::ToolPhase::Done, "");
        assert_eq!(
            summary(&[grep.clone(), fetch.clone(), fetch]),
            "Searched once · WebFetch ×2"
        );
    }
}
