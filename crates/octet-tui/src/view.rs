//! Drawing: the header, conversation, prompt box, sidebar and dialogs. The
//! state they draw is in `app`.
use crate::{
    app::App,
    mascot,
    registry::{COMMANDS, Cmd},
};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

// Warm charcoal and burgundy surfaces with readable, muted crimson accents.
// Amber remains reserved for warnings and permission decisions.
pub(crate) const BG: Color = Color::Rgb(22, 17, 19);
pub(crate) const PANEL: Color = Color::Rgb(31, 23, 26);
pub(crate) const FG: Color = Color::Rgb(234, 224, 218);
pub(crate) const MUTED: Color = Color::Rgb(174, 151, 154);
pub(crate) const EDGE: Color = Color::Rgb(88, 51, 61);
pub(crate) const ACCENT: Color = Color::Rgb(216, 124, 130);
pub(crate) const AMBER: Color = Color::Rgb(224, 180, 119);
const SELECTED: Color = Color::Rgb(64, 36, 44);
/// A count of rows or columns as a terminal coordinate, saturating instead
/// of wrapping.
fn cells(count: usize) -> u16 {
    u16::try_from(count).unwrap_or(u16::MAX)
}
fn card(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(EDGE))
        .title(Span::styled(title, Style::default().fg(MUTED)))
}
fn mode_chip(app: &App) -> Span<'static> {
    let active = app.conn.mode_for_turn.unwrap_or(app.conn.mode);
    let color = match active {
        octet_core::Mode::Ask => MUTED,
        octet_core::Mode::AcceptEdits | octet_core::Mode::Auto => ACCENT,
        octet_core::Mode::FullAccess => AMBER,
    };
    let text = match app.conn.mode_pending {
        Some(pending) => format!("{}…", pending.label()),
        None if active != app.conn.mode => {
            format!("{} → {} next", active.label(), app.conn.mode.label())
        }
        None => app.conn.mode.label().to_owned(),
    };
    Span::styled(text, Style::default().fg(color).bold())
}
pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    app.last_width = frame.area().width;
    paint(frame, app);
    if app.monochrome {
        without_colour(frame.buffer_mut());
    }
}
/// `NO_COLOR`: every cell takes the terminal's own colours. A selection, shown
/// only by its background, becomes reverse video instead.
fn without_colour(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        if cell.bg == SELECTED {
            cell.modifier.insert(Modifier::REVERSED);
        }
        cell.fg = Color::Reset;
        cell.bg = Color::Reset;
    }
}
fn paint(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    if area.width < 38 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(
                "Octet\n\nEnlarge the terminal to at least 38 × 12.\nPress Ctrl+C twice to quit.",
            )
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    // Laid out once per frame: its row count sizes the composer, and its
    // lines and cursor are drawn there.
    let draft = app.composer.editor.layout(draft_width(area.width));
    let compact = compact(area);
    let regions = Layout::vertical([
        Constraint::Length(if compact { 1 } else { 3 }),
        Constraint::Min(3),
        Constraint::Length(composer_height(draft.0.len(), chips(app).is_some())),
        Constraint::Length(1),
    ])
    .horizontal_margin(1)
    .vertical_margin(u16::from(!compact))
    .split(area);
    if compact {
        header_line(frame, regions[0], app);
    } else {
        header(frame, regions[0], app);
    }
    conversation(frame, regions[1], area.width, app);
    let cursor = composer(frame, regions[2], area.width, app, draft);
    status_line(frame, regions[3], app, compact);
    overlays(frame, area, app, cursor);
}
/// Below this many rows the header is one row and the layout has no vertical margin.
pub(crate) const COMPACT_BELOW: u16 = 30;
/// Short terminals trade the banner for transcript rows.
fn compact(area: Rect) -> bool {
    area.height < COMPACT_BELOW
}
/// The one-cell mark's colour: the mascot's state at a glance.
fn mark_color(state: mascot::State) -> Color {
    match state {
        mascot::State::Approval | mascot::State::Error => AMBER,
        mascot::State::Idle | mascot::State::Sleeping => MUTED,
        _ => ACCENT,
    }
}
/// The connection's status, or the pending approval, with its dot.
fn status_text(app: &App) -> String {
    format!(
        "● {}",
        if app.overlay.approvals.is_empty() {
            app.conn.status.as_str()
        } else {
            "approval needed"
        }
    )
}
/// Amber for an approval, muted once stopped, else the accent.
fn status_color(app: &App) -> Color {
    if !app.overlay.approvals.is_empty() {
        AMBER
    } else if app.conn.is_stopped() {
        MUTED
    } else {
        ACCENT
    }
}
/// The compact header: mark, name, mode, model and workspace on the left; status and usage right.
fn header_line(frame: &mut Frame, area: Rect, app: &App) {
    let status = status_text(app);
    // The status always shows in full; the usage joins it only where there is room.
    let room = (area.width / 2).max(cells(status.width()));
    let with_usage = format!("{status} · {}", app.conn.usage);
    let right = if !app.conn.usage.is_empty() && cells(with_usage.width()) <= room {
        with_usage
    } else {
        status
    };
    let right_width = cells(right.width());
    let [left, right_area] =
        Layout::horizontal([Constraint::Min(6), Constraint::Length(right_width)])
            .spacing(1)
            .areas(area);
    let mark = if app.monochrome { "◇" } else { "◆" };
    let model = format!("{} / {}", app.conn.engine, app.conn.model);
    let (first, second) = if area.width < 80 {
        (&app.conn.workspace, &model)
    } else {
        (&model, &app.conn.workspace)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{mark} "),
                Style::default().fg(mark_color(app.mascot_state())),
            ),
            Span::styled("Octet", Style::default().fg(ACCENT).bold()),
            Span::styled(" · ", Style::default().fg(MUTED)),
            mode_chip(app),
            Span::styled(format!(" · {first}  {second}"), Style::default().fg(MUTED)),
        ])),
        left,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(right, Style::default().fg(status_color(app))))
            .alignment(Alignment::Right),
        right_area,
    );
}
/// Banner on the left, status and usage on the right.
fn header(frame: &mut Frame, area: Rect, app: &App) {
    let status = status_text(app);
    // Status and usage keep the right edge, sized to their text so a narrow
    // terminal never cuts them; the banner takes the rest.
    let status_width = cells(status.width().max(app.conn.usage.width())).clamp(12, 22);
    let header = Layout::horizontal([Constraint::Min(6), Constraint::Length(status_width)])
        .spacing(1)
        .split(area);
    banner(frame, header[0], app);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(status, Style::default().fg(status_color(app)))),
            Line::from(Span::styled(
                app.conn.usage.clone(),
                Style::default().fg(MUTED),
            )),
        ])
        .alignment(Alignment::Right),
        header[1],
    );
}
/// The narrowest terminal that shows the Task sidebar.
pub(crate) const SIDEBAR_FROM: u16 = 112;
/// The columns of text the transcript holds at terminal `width`, as `conversation` lays it out; None before a frame.
pub(crate) fn transcript_room(width: u16, sidebar: bool) -> Option<u16> {
    // Margins 2, border 2, padding 2; the sidebar takes 29 and a 2-column gap.
    let frame = if width >= SIDEBAR_FROM && sidebar {
        37
    } else {
        6
    };
    (width > 0).then(|| width.saturating_sub(frame))
}
/// The transcript, and the sidebar on wide terminals.
fn conversation(frame: &mut Frame, area: Rect, width: u16, app: &mut App) {
    let columns = Layout::horizontal(if width >= SIDEBAR_FROM && app.sidebar {
        vec![Constraint::Min(40), Constraint::Length(29)]
    } else {
        vec![Constraint::Min(20)]
    })
    .spacing(2)
    .split(area);
    let transcript = columns[0];
    // Before the first message the conversation area stays blank, reserved
    // for what the conversation will fill.
    if !app.chat.entries.is_empty() {
        let inner = card(" Conversation ").inner(transcript);
        let lines = app.visible_lines(inner.width.saturating_sub(2), inner.height as usize);
        let mut block = card(" Conversation ");
        if app.chat.unseen {
            block = block.title_bottom(Line::from(Span::styled(
                " New output below · Ctrl+End follows ",
                Style::default().fg(ACCENT),
            )));
        }
        frame.render_widget(block, transcript);
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                x: inner.x + 1,
                width: inner.width.saturating_sub(2),
                ..inner
            },
        );
    }
    if columns.len() > 1 {
        sidebar(frame, columns[1], app);
    }
}
/// The prompt box and its popup; returns where the cursor goes.
fn composer(
    frame: &mut Frame,
    area: Rect,
    width: u16,
    app: &App,
    draft: (Vec<String>, (usize, usize)),
) -> (u16, u16) {
    let composer = card(composer_title(app))
        .border_style(Style::default().fg(if app.conn.is_running() { EDGE } else { ACCENT }));
    let inner = composer.inner(area);
    frame.render_widget(composer, area);
    let chips = chips(app);
    // At the minimum height the draft keeps its row; the hint, then the chips, give way.
    let hint_rows = u16::from(inner.height >= 2);
    let chip_rows = u16::from(chips.is_some() && inner.height >= 3);
    let input = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height - hint_rows - chip_rows,
        ..inner
    };
    let (lines, (col, row)) = draft;
    let offset = row.saturating_sub(input.height.saturating_sub(1) as usize);
    if app.composer.editor.text().is_empty() {
        frame.render_widget(
            Paragraph::new("Ask about this workspace, describe a change, or type /help…")
                .style(Style::default().fg(MUTED)),
            input,
        );
    } else {
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(offset)
                    .take(input.height as usize)
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            ),
            input,
        );
    }
    if let Some(chips) = chips.filter(|_| chip_rows == 1) {
        frame.render_widget(
            Paragraph::new(chips).style(Style::default().fg(ACCENT)),
            Rect {
                y: input.bottom(),
                height: 1,
                ..input
            },
        );
    }
    let (hint, color) = composer_hint(app, width);
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(color)),
        Rect {
            x: input.x,
            y: inner.bottom().saturating_sub(1),
            width: input.width,
            height: hint_rows,
        },
    );
    completion_popup(frame, area, app);
    (
        input.x + cells(col).min(input.width.saturating_sub(1)),
        input.y + cells(row - offset).min(input.height.saturating_sub(1)),
    )
}
/// The bottom line: the latest notice, or the standing hints.
fn status_line(frame: &mut Frame, area: Rect, app: &App, compact: bool) {
    frame.render_widget(
        Paragraph::new(if app.status_line.is_empty() {
            if compact {
                if app.conn.engine.offline() {
                    " Offline demo · F1 help · Ctrl+C ×2 quit".into()
                } else {
                    " F1 help · Ctrl+C ×2 quit".into()
                }
            } else if app.conn.engine.offline() {
                " Offline demo · no model calls   |   Ctrl+C twice to quit".into()
            } else {
                " Journal saved locally   |   F1 help   |   Ctrl+C twice to quit".into()
            }
        } else {
            app.status_line.clone()
        })
        .style(Style::default().fg(MUTED)),
        area,
    );
}
/// Help, the palette or an approval over everything; else the cursor.
fn overlays(frame: &mut Frame, area: Rect, app: &mut App, cursor: (u16, u16)) {
    if app.overlay.help {
        // Drawn over any approval, which must be seen again before a key
        // answers it.
        app.overlay.approval_hidden();
        help(frame, area, &mut app.overlay.help_scroll);
    } else if app.overlay.palette {
        app.overlay.approval_hidden();
        palette(
            frame,
            area,
            app.overlay.selection,
            &app.overlay.palette_query,
        );
    } else if let Some(asked) = app.overlay.approvals.front() {
        let count = app.overlay.approvals.len();
        approval(frame, area, asked, &mut app.overlay.approval_scroll, count);
        app.overlay.approval_drawn();
    } else {
        frame.set_cursor_position(cursor);
    }
}
/// The `@`, path or command suggestions, just above the prompt box.
fn completion_popup(frame: &mut Frame, composer: Rect, app: &App) {
    let Some(completion) = &app.composer.completion else {
        return;
    };
    let mut lines: Vec<Line> = completion
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let chosen = index == completion.selected;
            Line::from(Span::styled(
                format!(" {} {item}", if chosen { "›" } else { " " }),
                Style::default()
                    .fg(if chosen { ACCENT } else { FG })
                    .bg(if chosen { SELECTED } else { PANEL }),
            ))
        })
        .collect();
    if lines.is_empty() {
        let message = match (&completion.kind, &app.composer.files) {
            (crate::composer::Kind::File, crate::files::Files::Ready(_)) => " No matching files",
            (crate::composer::Kind::File, _) => " Indexing files…",
            (crate::composer::Kind::Model, _) if app.model_entries().is_empty() => {
                " No model list yet · /model refresh"
            }
            (crate::composer::Kind::Skill, _) if app.skill_entries().is_empty() => {
                " No skills yet · /model refresh fetches them"
            }
            _ => " No matches",
        };
        lines.push(Line::from(Span::styled(
            message,
            Style::default().fg(MUTED),
        )));
    }
    if let crate::files::Files::Ready(index) = &app.composer.files
        && index.capped
        && completion.kind == crate::composer::Kind::File
    {
        lines.push(Line::from(Span::styled(
            format!(" Indexed the first {} files", crate::files::LIMIT),
            Style::default().fg(MUTED),
        )));
    }
    let title = match completion.kind {
        crate::composer::Kind::File => " Files · Enter choose · Esc close ",
        crate::composer::Kind::Path => " Paths ",
        crate::composer::Kind::Command => " Commands ",
        crate::composer::Kind::Model => " Models ",
        crate::composer::Kind::Skill => " Skills · Enter choose ",
    };
    // On a short screen the popup is clipped; scroll so the selected row
    // stays in sight.
    let room = composer.y.saturating_sub(2) as usize;
    if completion.items.len() > room && room > 0 {
        let skip = completion.selected.saturating_sub(room - 1);
        lines = lines.into_iter().skip(skip).take(room).collect();
    }
    let height = cells(lines.len()).saturating_add(2);
    // At least 64 columns, and as wide as its longest line (the marker,
    // the borders and a space beside it), so a long model or skill line is
    // not cut while the screen has room.
    let longest = lines.iter().map(Line::width).max().unwrap_or(0);
    let area = Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width: composer.width.min(cells(longest).saturating_add(3).max(64)),
        height: height.min(composer.y),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(card(title))
            .style(Style::default().bg(PANEL)),
        area,
    );
}
/// The prompt box's title: whether this prompt goes now or after the turn.
pub(crate) fn composer_title(app: &App) -> &'static str {
    if app.conn.is_running() {
        " Next prompt "
    } else {
        " Prompt "
    }
}
/// What waits to go with the next prompt: attachments, images, the queue.
pub(crate) fn chips(app: &App) -> Option<String> {
    let mut parts: Vec<String> = crate::composer::chip(&app.composer.attachments)
        .into_iter()
        .collect();
    match app.composer.images.as_slice() {
        [] => {}
        [one] => parts.push(format!("+ image {}", one.name)),
        many => parts.push(format!("+{} images", many.len())),
    }
    if !app.composer.queue.is_empty() {
        parts.push(format!("{} queued", app.composer.queue.len()));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}
/// The hint row: what Enter does now, then the next keys that fit.
pub(crate) fn composer_hint(app: &App, width: u16) -> (String, Color) {
    let draft = app.composer.editor.text();
    if let Some(engine) = crate::commands::foreign_skill(app, draft) {
        // Enter switches only when a reconnect may go; while connecting it says so itself.
        match crate::commands::can_reconnect(app) {
            Ok(()) => {
                let name = draft.split(char::is_whitespace).next().unwrap_or(draft);
                return (
                    format!("Enter switches to {engine}, then runs {name}"),
                    AMBER,
                );
            }
            Err(crate::registry::TURN_OPEN) => {
                return (format!("Finish the turn to switch to {engine}"), AMBER);
            }
            Err(_) => {}
        }
    }
    let keys: &[&str] = if app.conn.is_running() {
        &["Enter queue", "Esc cancel turn", "Ctrl+P commands"]
    } else if width < 80 {
        // A narrow screen is often a phone, which rarely sends Alt+Enter.
        &["Enter send", "Ctrl+J newline"]
    } else {
        &[
            "Enter send",
            "Alt+Enter newline",
            "↑ history",
            "Ctrl+P commands",
        ]
    };
    let shown = if width >= 80 { keys.len() } else { 2 };
    (keys[..shown].join(" · "), MUTED)
}
/// The width the draft wraps at: margin, border and padding take three
/// columns on each side.
fn draft_width(terminal_width: u16) -> usize {
    terminal_width.saturating_sub(6) as usize
}
/// The composer's height: borders, 1–4 draft rows, the chips row when there is one, the hint row.
fn composer_height(draft_rows: usize, chips: bool) -> u16 {
    2 + cells(draft_rows.clamp(1, 4)) + u16::from(chips) + 1
}
/// The top-left banner, after Claude Code's: the mini Octet (or a text mark
/// without colour) beside three lines. The permission mode leads its line so
/// a long model name can never push it off a narrow screen.
fn banner(frame: &mut Frame, area: Rect, app: &App) {
    let lines = vec![
        Line::from(vec![
            Span::styled("Octet", Style::default().fg(ACCENT).bold()),
            Span::styled(
                format!(" v{} · preview", env!("CARGO_PKG_VERSION")),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            mode_chip(app),
            Span::styled(
                format!("  ·  {}  /  {}", app.conn.engine, app.conn.model),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            Span::styled(app.mascot_state().label(), Style::default().fg(ACCENT)),
            Span::styled(
                format!("  ·  {}", app.conn.workspace),
                Style::default().fg(MUTED),
            ),
        ]),
    ];
    let (mark_width, mark) = if app.monochrome {
        (
            1,
            vec![Line::from(Span::styled("◇", Style::default().fg(ACCENT)))],
        )
    } else {
        (9, mascot::mini(app.mascot_state(), BG))
    };
    let [mark_area, _, text] = Layout::horizontal([
        Constraint::Length(mark_width),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(mark), mark_area);
    frame.render_widget(Paragraph::new(lines), text);
}
/// The Task card: the goal, what is happening now, and what is queued.
fn sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let block = card(" Task ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width.saturating_sub(1)).max(1);
    let head = |text: &'static str| {
        Line::from(Span::styled(format!(" {text}"), Style::default().fg(MUTED)))
    };
    let body = |text: String| Line::from(format!(" {text}"));
    let mut lines = vec![Line::default(), head("GOAL")];
    match app.goals.goal() {
        Some(goal) => {
            let mut rows = transcript::wrap(&crate::text::clean(goal.objective()), width);
            if rows.len() > 4 {
                rows.truncate(4);
                rows[3].push('…');
            }
            lines.extend(rows.into_iter().map(body));
            lines.push(Line::from(Span::styled(
                format!(
                    " {} · {}/{} turns",
                    goal.status(),
                    goal.turns(),
                    octet_core::goal::MAX_GOAL_TURNS
                ),
                Style::default().fg(MUTED),
            )));
        }
        None => lines.push(body("No goal · /goal sets one".into())),
    }
    lines.extend([Line::default(), head("NOW")]);
    lines.extend(
        transcript::wrap(&app.now_line(), width)
            .into_iter()
            .take(2)
            .map(body),
    );
    lines.extend([Line::default(), head("QUEUED")]);
    if app.composer.queue.is_empty() {
        lines.push(body("Nothing queued".into()));
    }
    for prompt in app.composer.queue.iter().take(3) {
        let first = crate::text::clean(prompt.display().lines().next().unwrap_or_default());
        lines.push(body(transcript::wrap(&first, width).swap_remove(0)));
    }
    if app.composer.queue.len() > 3 {
        lines.push(body(format!("+{} more", app.composer.queue.len() - 3)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
fn modal(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}
/// The help screen: keys, then every command's usage, then the rest.
pub(crate) fn help_lines() -> Vec<String> {
    let keys = [
        "Octet terminal preview",
        "",
        "Enter send · Alt+Enter / Ctrl+J newline",
        "Arrows, Home, End edit; ↑ ↓ browse prompt history",
        "Ctrl+U clear draft · PgUp/PgDn scroll conversation",
        "Ctrl+End follow · Esc/Ctrl+C cancel turn",
        "Ctrl+C twice quit · Ctrl+Z suspend (return with fg)",
        "Ctrl+P commands · Ctrl+G write the prompt in $EDITOR",
        "Ctrl+O show or hide tool details",
        "F2 show or hide the Task sidebar",
        "!cmd run and attach output · !!cmd run only · Esc stops",
        "@ mention a file · Tab completes paths and /commands",
        "",
    ];
    let rest = [
        "/approval-demo: offline permission dialog",
        "",
        "Approval: A allow once · D/Esc deny",
        "Journals retain older output beyond the viewport.",
        "Esc or F1 closes help",
    ];
    keys.into_iter()
        .chain(
            COMMANDS
                .iter()
                .filter(|spec| spec.id != Cmd::Help)
                .map(|spec| spec.usage),
        )
        .chain(rest)
        .map(str::to_owned)
        .collect()
}
/// The help screen: as tall as its wrapped lines allow, scrolled when the
/// terminal is shorter. How to scroll and close is on the border, so it
/// shows at any size.
fn help(frame: &mut Frame, area: Rect, scroll: &mut u16) {
    let width = 76.min(area.width.saturating_sub(4));
    let inner = usize::from(width.saturating_sub(2));
    // Wrapped here, once, so the rows counted are the rows drawn.
    let rows = wrapped(&help_lines(), inner);
    let area = modal(area, 76, cells(rows.len()).saturating_add(2));
    let visible = area.height.saturating_sub(2);
    *scroll = (*scroll).min(cells(rows.len()).saturating_sub(visible));
    frame.render_widget(Clear, area);
    let title = if cells(rows.len()) > visible {
        " Help · PgUp/PgDn scroll · Esc closes "
    } else {
        " Help · Esc closes "
    };
    frame.render_widget(
        Paragraph::new(rows.join("\n"))
            .block(card(title))
            .style(Style::default().bg(PANEL).fg(FG))
            .scroll((*scroll, 0)),
        area,
    );
}
/// Keys the palette also offers, after the commands.
pub(crate) const PALETTE_KEYS: [(&str, &str); 4] = [
    ("Ctrl+G", "Write the prompt in $EDITOR"),
    ("@", "Mention a file"),
    ("!", "Run a shell command"),
    ("Ctrl+O", "Show or hide tool details"),
];
/// The palette's rows: every command, then the keys it also offers.
pub(crate) fn palette_entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    COMMANDS
        .iter()
        .map(|spec| (spec.name, spec.summary))
        .chain(PALETTE_KEYS.iter().copied())
}
/// The palette rows whose name or summary contains `query`, ignoring case.
pub(crate) fn palette_matches(query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    palette_entries()
        .enumerate()
        .filter(|(_, (name, summary))| format!("{name} {summary}").to_lowercase().contains(&query))
        .map(|(index, _)| index)
        .collect()
}
fn palette(frame: &mut Frame, area: Rect, selected: usize, query: &str) {
    let name_width = palette_entries()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let description_width = palette_entries()
        .map(|(_, description)| description.width())
        .max()
        .unwrap_or(0);
    // Width: borders, the marker and spaces around each column. Height:
    // borders and a blank line above and below the list. The height is the
    // full list's, so the modal does not jump as the filter narrows it.
    let area = modal(
        area,
        cells(name_width + description_width + 7),
        cells(palette_entries().count()).saturating_add(4),
    );
    frame.render_widget(Clear, area);
    let matches = palette_matches(query);
    // On a short screen the list scrolls to keep the selected row in sight.
    let window = usize::from(area.height.saturating_sub(4)).max(1);
    let first = selected.saturating_sub(window - 1);
    let mut lines = vec![Line::default()];
    if matches.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No matching commands",
            Style::default().fg(MUTED).bg(PANEL),
        )));
    }
    let entries: Vec<(&str, &str)> = palette_entries().collect();
    for (row, &index) in matches.iter().enumerate().skip(first).take(window) {
        let (command, description) = entries[index];
        let chosen = row == selected;
        lines.push(Line::from(Span::styled(
            format!(
                " {} {:<name_width$} {}",
                if chosen { "›" } else { " " },
                command,
                description
            ),
            Style::default()
                .fg(if chosen { ACCENT } else { FG })
                .bg(if chosen { SELECTED } else { PANEL }),
        )));
    }
    let title = if query.is_empty() {
        " Commands · ↑ ↓ choose · Enter run · Esc close ".to_owned()
    } else {
        format!(" Commands · \"{query}\" · ↑ ↓ · Enter run · Esc close ")
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(card(&title))
            .style(Style::default().bg(PANEL)),
        area,
    );
}
/// `lines`, each wrapped to `width` as the transcript wraps.
fn wrapped<S: AsRef<str>>(lines: &[S], width: usize) -> Vec<String> {
    lines
        .iter()
        .flat_map(|line| transcript::wrap(line.as_ref(), width))
        .collect()
}
/// The approval's lead: what it would do, its command (3 lines at most), its files (5 at most),
/// where, and what allowing grants.
pub(crate) fn summary_lines(summary: &octet_core::ApprovalSummary) -> Vec<String> {
    let mut lines = vec![summary.action.clone()];
    if let Some(command) = &summary.command {
        let all: Vec<&str> = command.lines().collect();
        for (n, line) in all.iter().take(3).enumerate() {
            lines.push(format!("{} {line}", if n == 0 { "$" } else { " " }));
        }
        if all.len() > 3 {
            lines.push("  …".into());
        }
    }
    lines.extend(summary.files.iter().take(5).cloned());
    if summary.files.len() > 5 {
        lines.push(format!("+{} more", summary.files.len() - 5));
    }
    if let Some(cwd) = &summary.cwd {
        lines.push(format!("in {cwd}"));
    }
    lines.push(format!("Allows: {}", summary.scope));
    lines
}
/// `rows` within `room`: the head, one `…` for the cut middle and the last `tail` rows; a tight room drops the
/// mark, then the head, then the tail's own first rows, so the last row always stays.
fn fit_lead(rows: Vec<String>, room: usize, tail: usize) -> Vec<String> {
    if rows.len() <= room {
        return rows;
    }
    let tail = tail.min(room);
    let free = room - tail;
    let head = if free > 1 { free - 1 } else { free };
    let mut fitted: Vec<String> = rows[..head].to_vec();
    if free > 1 {
        fitted.push("…".into());
    }
    fitted.extend_from_slice(&rows[rows.len() - tail..]);
    fitted
}
/// The fewest rows the approval's full request keeps: its borders and one line.
const REQUEST_ROWS: u16 = 3;
fn approval(
    frame: &mut Frame,
    area: Rect,
    asked: &crate::app::Asked,
    scroll: &mut u16,
    count: usize,
) {
    let area = modal(area, 86, 24);
    frame.render_widget(Clear, area);
    // Framed where there is room, so on a wide screen it reads as a dialog
    // over the conversation rather than a column of it; a small screen
    // keeps every row for the request.
    let border = Block::default()
        .borders(if area.height >= 16 && area.width >= 50 {
            Borders::ALL
        } else {
            Borders::NONE
        })
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(AMBER))
        .style(Style::default().bg(PANEL));
    let inner = border.inner(area);
    frame.render_widget(border, area);
    let area = inner;
    let width = usize::from(area.width.saturating_sub(2));
    let lines = summary_lines(&asked.summary);
    // The folder (when there is one) and the grant are the rows that must stay.
    let kept = wrapped(
        &lines[lines.len() - 1 - usize::from(asked.summary.cwd.is_some())..],
        width,
    )
    .len();
    // The heading, the keys and the smallest request box (a border and one line) keep their
    // rows, so the full request can always be scrolled to before it is allowed.
    let lead = fit_lead(
        wrapped(&lines, width),
        usize::from(area.height.saturating_sub(3 + REQUEST_ROWS)),
        kept,
    );
    // The keys keep their rows; the lead comes next; the full request takes what is left.
    let [heading, lead_area, request, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(cells(lead.len())),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(format!(
            " Permission required · #{} · {count} pending",
            asked.id
        ))
        .style(Style::default().fg(AMBER).bg(PANEL).bold()),
        heading,
    );
    frame.render_widget(
        Paragraph::new(
            lead.iter()
                .map(|l| Line::from(format!(" {l}")))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(FG).bg(PANEL)),
        lead_area,
    );
    let inner = usize::from(request.width.saturating_sub(2));
    let rows = wrapped(&asked.detail.lines().collect::<Vec<_>>(), inner);
    let visible = request.height.saturating_sub(2);
    *scroll = (*scroll).min(cells(rows.len()).saturating_sub(visible));
    if request.height >= REQUEST_ROWS {
        frame.render_widget(
            Paragraph::new(rows.join("\n"))
                .scroll((*scroll, 0))
                .block(card(" Full request · PgUp/PgDn "))
                .style(Style::default().bg(PANEL).fg(MUTED)),
            request,
        );
    }
    frame.render_widget(
        Paragraph::new("\n A  Allow once     D / Esc  Deny     Ctrl+C  Cancel turn")
            .style(Style::default().fg(AMBER).bg(PANEL)),
        keys,
    );
}

pub(crate) mod transcript;

#[cfg(test)]
mod tests;
