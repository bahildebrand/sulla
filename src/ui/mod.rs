pub mod board;
pub mod keys;
pub mod progress;
pub mod review;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, InputKind, Mode, NewBeadForm, View};
use crate::beads::State;
use crate::config::AgentKind;
use crate::health::Status;
use review::{DLine, Diff};

pub fn draw(f: &mut Frame, app: &App) {
    let [main, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
    match &app.view {
        View::Board => board::render(f, main, app),
        View::Progress(id) => progress::render(f, main, app, id),
        View::Review(d) => {
            let bead = app.all.iter().find(|b| b.id == d.id);
            let title = bead.map(|b| b.title.as_str()).unwrap_or("");
            review::render(
                f,
                main,
                d,
                review::Header {
                    title,
                    loc: app.slot_location(&d.id).as_deref(),
                    ai: app.ai_reviews.get(&d.id),
                    conflict: app.conflicts.get(&d.id).map(|c| (c, app.base.as_str())),
                    check: app.checks.get(&d.id),
                    released: bead.is_some_and(|b| {
                        b.status != "closed" && b.state() == Some(State::Released)
                    }),
                    marked: match app.mode {
                        Mode::Input {
                            kind: InputKind::Comment(n),
                            ..
                        } => d.comment_range(n),
                        _ => None,
                    },
                },
            )
        }
    }
    status_line(f, status, app);
    if let Mode::Help { scroll } = app.mode {
        help(f, scroll);
    }
    if matches!(app.mode, Mode::Health) {
        health(f, app);
    }
    if let Mode::NewBead(form) = &app.mode {
        new_bead(f, form);
    }
    if let Mode::Inspect { id, text, scroll } = &app.mode {
        inspect(f, id, text, *scroll);
    }
    if let Mode::Confirm { msg, .. } = &app.mode {
        confirm(f, msg);
    }
    if let (Mode::Input { kind, buf }, View::Review(d)) = (&app.mode, &app.view) {
        match kind {
            InputKind::Comment(n) => {
                if let Some(range) = d.comment_range(*n) {
                    comment(f, d, range, buf, false);
                }
            }
            InputKind::EditComment(i) => {
                let cursor = d.files.get(d.sel).map_or(0, |x| x.cursor);
                let range = d
                    .comments
                    .get(*i)
                    .and_then(|c| d.comment_span(c))
                    .filter(|&(fi, _, _)| fi == d.sel)
                    .map_or((cursor, cursor), |(_, s, e)| (s, e));
                comment(f, d, range, buf, true);
            }
            _ => {}
        }
    }
}

fn status_line(f: &mut Frame, area: Rect, app: &App) {
    let bar = Style::new().fg(theme::SUBTEXT).bg(theme::PANEL);
    f.buffer_mut().set_style(area, bar);
    let tag = |s: &str, c: Color| theme::pill(s, c);
    let hints = |pairs: &[(&str, &str)]| {
        let mut v = vec![Span::raw(" ")];
        v.extend(theme::hint(pairs));
        v
    };
    let mut left: Vec<Span> = match &app.mode {
        Mode::Health => [
            vec![tag("HEALTH", theme::TEAL)],
            hints(&[("r", "recheck"), ("any", "close")]),
        ]
        .concat(),
        Mode::Help { .. } => [
            vec![tag("HELP", theme::LAVENDER)],
            hints(&[("j k", "scroll"), ("gg G", "jump"), ("any", "close")]),
        ]
        .concat(),
        Mode::Normal => {
            let mut spans = vec![tag("NORMAL", theme::BLUE)];
            if let Some(n) = app.count {
                spans.push(Span::styled(
                    format!(" {n}"),
                    Style::new().fg(theme::YELLOW).bold(),
                ));
            }
            spans.push(Span::styled(
                format!(" {}", app.status),
                Style::new().fg(theme::TEXT),
            ));
            spans
        }
        Mode::Insert => [
            vec![tag("INSERT", theme::GREEN)],
            hints(&[("Esc Esc / Ctrl-]", "leave")]),
        ]
        .concat(),
        Mode::Input {
            kind: InputKind::Comment(_),
            ..
        } => [
            vec![tag("COMMENT", theme::MAUVE)],
            hints(&[("Enter", "add"), ("Ctrl-u", "clear"), ("Esc", "cancel")]),
        ]
        .concat(),
        Mode::Input {
            kind: InputKind::EditComment(_),
            ..
        } => [
            vec![tag("EDIT", theme::MAUVE)],
            hints(&[("Enter", "save"), ("Ctrl-u", "clear"), ("Esc", "cancel")]),
        ]
        .concat(),
        Mode::Input { kind, buf } => {
            let (label, color, prompt) = match kind {
                InputKind::Command => ("COMMAND", theme::PEACH, ":".to_string()),
                InputKind::Search(_) => ("FILTER", theme::YELLOW, "/".to_string()),
                InputKind::Feedback(id) => ("FEEDBACK", theme::SKY, format!("to {id}: ")),
                InputKind::Retry(id) => ("RETRY", theme::PEACH, format!("{id}, why did it fail: ")),
                InputKind::Comment(_) | InputKind::EditComment(_) => {
                    ("", theme::MAUVE, String::new())
                }
            };
            vec![
                tag(label, color),
                Span::styled(format!(" {prompt}"), Style::new().fg(color).bold()),
                Span::styled(buf.clone(), Style::new().fg(theme::TEXT)),
                Span::styled("▏", Style::new().fg(color)),
            ]
        }
        Mode::NewBead(_) => [
            vec![tag("NEW", theme::GREEN)],
            hints(&[
                ("Tab", "next field"),
                ("Enter", "create"),
                ("Esc", "cancel"),
            ]),
        ]
        .concat(),
        Mode::Inspect { .. } => [
            vec![tag("INSPECT", theme::MAUVE)],
            hints(&[
                ("j k", "scroll"),
                ("p", "progress"),
                ("r", "review"),
                ("l", "log"),
                ("h", "timeline"),
                ("q", "close"),
            ]),
        ]
        .concat(),
        Mode::Confirm { .. } => [
            vec![tag("CONFIRM", theme::YELLOW)],
            hints(&[("y", "confirm"), ("any", "cancel")]),
        ]
        .concat(),
    };
    let mut right: Vec<Span> = Vec::new();
    let flag = |right: &mut Vec<Span<'static>>, s: Span<'static>| {
        right.push(s);
        right.push(Span::raw(" "));
    };
    if app.filter.is_active() {
        flag(&mut right, tag("FILTER", theme::YELLOW));
        flag(
            &mut right,
            Span::styled(
                format!("/{}", app.filter.query),
                Style::new().fg(theme::YELLOW),
            ),
        );
    }
    if let Some(id) = &app.chain_filter {
        flag(&mut right, tag("CHAIN", theme::YELLOW));
        flag(
            &mut right,
            Span::styled(id.clone(), Style::new().fg(theme::YELLOW)),
        );
    }
    if app.paused {
        flag(&mut right, tag("⏸ PAUSED", theme::PEACH));
    }
    if let Some(d) = app.backoff.remaining(std::time::Instant::now()) {
        flag(
            &mut right,
            tag(
                &format!("RATE LIMITED {}", crate::scheduler::fmt_duration(d)),
                theme::RED,
            ),
        );
    }
    let mut segments: Vec<(u8, Vec<Span<'static>>)> = Vec::new();
    if !app.run_usage.is_empty() {
        segments.push((
            1,
            vec![Span::styled(
                app.run_usage.short(),
                Style::new().fg(theme::OVERLAY),
            )],
        ));
    }
    segments.push((
        2,
        vec![Span::styled(
            format!("{} · {}", app.backend_name(), app.sandbox_name()),
            Style::new().fg(theme::OVERLAY),
        )],
    ));
    segments.push((6, meter(app.running_count(), app.cfg.max_agents)));
    segments.push((
        4,
        vec![
            Span::styled("base ", Style::new().fg(theme::OVERLAY)),
            Span::styled(app.base.clone(), Style::new().fg(theme::MAUVE).bold()),
        ],
    ));
    segments.push((
        5,
        vec![
            theme::key("?"),
            Span::styled(" help", Style::new().fg(theme::OVERLAY)),
        ],
    ));
    const BRAND: u8 = 3;
    segments.push((
        BRAND,
        vec![Span::styled(
            " ✦ sulla ",
            Style::new()
                .fg(theme::CRUST)
                .bg(theme::MAUVE)
                .add_modifier(Modifier::BOLD),
        )],
    ));
    let width_of = |v: &[Span]| v.iter().map(Span::width).sum::<usize>();
    let left_w = width_of(&left);
    let budget = (area.width as usize).saturating_sub(left_w + 1);
    let fixed = width_of(&right) + 1;
    while fixed + segments.iter().map(|(_, v)| width_of(v) + 3).sum::<usize>() > budget
        && segments.len() > 1
    {
        let lowest = segments.iter().map(|(p, _)| *p).min().unwrap_or_default();
        segments.retain(|(p, _)| *p != lowest);
    }
    let mut first = true;
    for (p, seg) in segments {
        if p == BRAND {
            right.push(Span::raw(" "));
        } else if !first {
            right.push(Span::styled(" │ ", Style::new().fg(theme::LINE)));
        }
        first = false;
        right.extend(seg);
    }
    let width: usize = right.iter().map(|s| s.width()).sum();
    let width = (width as u16).min(area.width);
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(width)]).areas(area);
    left.insert(0, Span::raw(""));
    f.render_widget(Line::from(left), l);
    f.render_widget(Line::from(right).right_aligned(), r);
}

fn meter(running: usize, max: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if max <= 8 {
        let on = Style::new().fg(theme::GREEN);
        let off = Style::new().fg(theme::LINE);
        spans.push(Span::styled("●".repeat(running.min(max)), on));
        spans.push(Span::styled("○".repeat(max.saturating_sub(running)), off));
        spans.push(Span::raw(" "));
    }
    let color = if running > 0 {
        theme::GREEN
    } else {
        theme::OVERLAY
    };
    spans.push(Span::styled(
        format!("{running}/{max}"),
        Style::new().fg(color).bold(),
    ));
    spans.push(Span::styled(" agents", Style::new().fg(theme::OVERLAY)));
    spans
}

const HELP: &[(&str, &str)] = &[
    ("Global", ""),
    ("?", "help (j k / Ctrl-d Ctrl-u / gg G scroll)"),
    (
        "! / :health",
        "health of bd, agent, kache, incus and why nothing starts",
    ),
    (
        ":",
        "command (:q :q! :new <title> :max <n> :pause :resume :refresh :kill <id> :setup [id] :chat [new] :health :browse [bead|slot])",
    ),
    ("Board", ""),
    ("h j k l / gg G", "move"),
    ("z / Esc", "zoom focused column / unzoom"),
    (
        "/",
        "filter: words match title/id, label:x (l:x), p:1 p:0-2 p:1,3",
    ),
    ("Esc", "clear filter (when not zoomed)"),
    (
        "L",
        "toggle chain filter: only the selected bead, its blockers/parents (▲) and dependents (▼)",
    ),
    (
        "Enter / p / r",
        "open / progress (reviewer if reviewing) / review",
    ),
    (
        "i",
        "inspect bead (description, labels, deps), l there shows session log",
    ),
    (
        "H",
        "bead timeline: time per state, review rounds, ai verdicts (h from inspect)",
    ),
    ("e", "open $EDITOR in bead worktree"),
    ("E", "open a shell in the bead's container, or its worktree"),
    (
        "n",
        "new bead popup (title, description, acceptance criteria, priority, agent, preset, start state)",
    ),
    ("c / C", "open conversation agent / start a fresh one"),
    ("+ -", "raise / lower priority"),
    ("X", "kill agent and hold bead, or stop ai reviewer"),
    ("D", "discard worktree + branch, hold bead"),
    ("dd", "delete bead permanently (also worktree + branch)"),
    ("F", "retry from scratch with a note for the agent"),
    ("s", "release held or backlog bead to pending"),
    ("S", "release every backlog bead to pending"),
    ("B", "move bead to backlog (not auto scheduled)"),
    ("P", "pause / resume auto scheduling of new beads"),
    ("R", "mark ready for review"),
    ("V", "start ai reviewer on a bead in review"),
    (
        "A",
        "launch / resume agent (blocked beads are queued instead)",
    ),
    ("t / T", "show pre-merge check output / rerun checks"),
    ("U", "revise an Awaiting PR bead from its new PR feedback"),
    ("o", "open the bead's GitHub PR in the default browser"),
    (
        "w",
        "release a Review or Awaiting PR bead's worktree so you can work on its branch, or reclaim a released one",
    ),
    ("q", "quit"),
    ("Progress", ""),
    ("i / a", "insert mode (type into agent, resumes if exited)"),
    ("Esc Esc / Ctrl-]", "leave insert mode"),
    ("j k / Ctrl-u Ctrl-d / gg G", "scroll"),
    ("X R r A", "kill / mark review / review / relaunch"),
    ("e", "open $EDITOR in bead worktree"),
    ("E", "open a shell in the bead's container, or its worktree"),
    ("q / Esc", "back to review if opened from it, else board"),
    ("Review", ""),
    ("j k / Ctrl-d Ctrl-u / gg G", "move line cursor"),
    ("<n>j / <n>k", "move n lines"),
    ("Tab J / S-Tab K / ]f [f", "next / prev file"),
    ("c / x", "comment on line / delete its comments"),
    ("<n>c", "comment on n lines from the cursor (10c)"),
    ("C", "edit comment on line (yours or unsent ai)"),
    ("S", "send all comments to agent (incl. unsent ai)"),
    ("y / n", "accept / reject hunk (whole file on header)"),
    ("Y / N", "accept / reject whole file"),
    (
        "m",
        "rebase + fast-forward merge (blocked unless checks pass)",
    ),
    ("M", "force merge despite failing, stale or running checks"),
    (
        "P",
        "publish: push branch and open a GitHub PR (workflow = \"pr\")",
    ),
    ("o", "open the bead's GitHub PR in the default browser"),
    ("U", "revise an Awaiting PR bead from its new PR feedback"),
    (
        "w",
        "release the worktree so you can work on the branch yourself, or reclaim it",
    ),
    ("t / T", "show pre-merge check output / rerun checks"),
    ("V", "start ai reviewer, its comments show inline"),
    ("s", "send feedback to agent"),
    ("D", "discard worktree + branch, hold bead"),
    ("F", "retry from scratch with a note for the agent"),
    ("p / a", "swap to agent view (keeps comments, r to return)"),
    ("i", "swap to agent view in insert mode"),
    ("R", "reload diff"),
    ("e", "open $EDITOR in bead worktree, reloads diff after"),
    (
        "E",
        "open a shell in the bead's container or worktree, reloads diff after",
    ),
];

const HELP_KEY_WIDTH: usize = 26;

fn help_width(screen: u16) -> u16 {
    screen.saturating_sub(4).min(104)
}

fn help_lines(width: u16) -> Vec<Line<'static>> {
    let inner = help_width(width).saturating_sub(6) as usize;
    let desc_w = inner.saturating_sub(HELP_KEY_WIDTH).max(16);
    let mut lines = Vec::new();
    for (k, d) in HELP {
        if d.is_empty() {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(theme::rule(k, theme::MAUVE, inner));
            continue;
        }
        let chips = theme::keys(k);
        let chips_w: usize = chips.iter().map(Span::width).sum();
        let mut first = chips;
        let pad = HELP_KEY_WIDTH.saturating_sub(chips_w).max(2);
        let wrap_w = if chips_w + 2 > HELP_KEY_WIDTH {
            inner.saturating_sub(chips_w + 2).max(16)
        } else {
            desc_w
        };
        let mut rows = theme::wrap(d, wrap_w).into_iter();
        first.push(Span::raw(" ".repeat(pad)));
        first.push(Span::styled(
            rows.next().unwrap_or_default(),
            Style::new().fg(theme::TEXT),
        ));
        lines.push(Line::from(first));
        for r in rows {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(HELP_KEY_WIDTH)),
                Span::styled(r, Style::new().fg(theme::TEXT)),
            ]));
        }
    }
    lines
}

pub fn help_max_scroll((width, height): (u16, u16)) -> u16 {
    let rows = help_lines(width).len() as u16;
    rows.saturating_sub(height.saturating_sub(5))
}

fn screen(f: &Frame) -> Rect {
    let [screen, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
    screen
}

fn help(f: &mut Frame, scroll: u16) {
    let size = (f.area().width, f.area().height);
    let lines = help_lines(size.0);
    let outer = screen(f);
    let height = (lines.len() as u16 + 4).min(outer.height.saturating_sub(2));
    let area = theme::centered(outer, help_width(size.0), height);
    let inner = theme::popup(
        f,
        area,
        theme::MAUVE,
        theme::popup_title("✦", "sulla keys", theme::MAUVE),
    );
    let [body] = Layout::vertical([Constraint::Fill(1)])
        .vertical_margin(1)
        .areas(inner);
    f.render_widget(
        Paragraph::new(lines).scroll((scroll.min(help_max_scroll(size)), 0)),
        body,
    );
}

fn health(f: &mut Frame, app: &App) {
    let width = f.area().width.saturating_sub(4).min(92);
    let inner_w = width.saturating_sub(6) as usize;
    let mut lines = vec![theme::rule("Tools", theme::TEAL, inner_w)];
    let row = |name: &str, glyph: &str, mark: &str, color: Color, text: String| {
        Line::from(vec![
            Span::styled(format!("{name:<10}"), Style::new().fg(theme::TEXT).bold()),
            Span::styled(format!("{glyph} {mark:<8}"), Style::new().fg(color).bold()),
            Span::styled(text, Style::new().fg(theme::SUBTEXT)),
        ])
    };
    match &app.health {
        None => lines.push(Line::styled(
            "checking…",
            Style::new().fg(theme::OVERLAY).italic(),
        )),
        Some(tools) => {
            for t in tools {
                let (glyph, mark, color, text) = match &t.status {
                    Status::Ok(v) => ("●", "ok", theme::GREEN, v.clone()),
                    Status::Missing => (
                        "✗",
                        "missing",
                        theme::RED,
                        format!("`{}` not found on PATH", t.bin),
                    ),
                    Status::Failed(e) => ("✗", "error", theme::RED, e.clone()),
                    Status::Off(why) => ("○", "off", theme::MUTED, why.clone()),
                };
                lines.push(row(&t.name, glyph, mark, color, text));
            }
        }
    }
    if let Some(e) = &app.bd_error {
        lines.push(row("bd poll", "✗", "error", theme::RED, e.clone()));
    }
    lines.push(Line::raw(""));
    lines.push(theme::rule("Scheduler", theme::TEAL, inner_w));
    let mut agents = meter(app.running_count(), app.cfg.max_agents);
    agents.push(Span::styled(" running", Style::new().fg(theme::OVERLAY)));
    lines.push(Line::from(agents));
    lines.push(match app.idle() {
        None => Line::styled("▶ starting work", Style::new().fg(theme::GREEN).bold()),
        Some(i) => Line::from(vec![
            Span::styled("◼ idle  ", Style::new().fg(theme::PEACH).bold()),
            Span::styled(i.describe(), Style::new().fg(theme::TEXT)),
        ]),
    });
    let rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner_w.max(1)).max(1))
        .sum();
    let outer = screen(f);
    let area = theme::centered(outer, width, rows as u16 + 4);
    let inner = theme::popup(
        f,
        area,
        theme::TEAL,
        theme::popup_title("♥", "health", theme::TEAL),
    );
    let [body] = Layout::vertical([Constraint::Fill(1)])
        .vertical_margin(1)
        .areas(inner);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

fn inspect_line(l: &str) -> Line<'static> {
    let text = l.trim_end().to_string();
    let word = text.trim().trim_end_matches(':');
    let is_heading = !word.is_empty()
        && !text.starts_with(' ')
        && word.chars().any(|c| c.is_ascii_alphabetic())
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == ' ' || c == '_' || c.is_ascii_digit());
    if is_heading {
        theme::heading(&format!(" {text}"), theme::MAUVE)
    } else if text.starts_with("$ ") {
        Line::styled(text, Style::new().fg(theme::PEACH).bold())
    } else {
        Line::styled(text, Style::new().fg(theme::TEXT))
    }
}

fn inspect(f: &mut Frame, id: &str, text: &str, scroll: u16) {
    let lines: Vec<Line> = text.lines().map(inspect_line).collect();
    let outer = screen(f);
    let width = f.area().width.saturating_sub(4).min(100);
    let inner_w = width.saturating_sub(6).max(1) as usize;
    let rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner_w).max(1))
        .sum();
    let area = theme::centered(outer, width, rows as u16 + 4);
    let inner = theme::popup(
        f,
        area,
        theme::MAUVE,
        theme::popup_title("◈", id, theme::MAUVE),
    );
    let [body] = Layout::vertical([Constraint::Fill(1)])
        .vertical_margin(1)
        .areas(inner);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        body,
    );
}

fn confirm(f: &mut Frame, msg: &str) {
    let question = msg.trim_end().trim_end_matches("y/n").trim_end();
    let width = f.area().width.saturating_sub(4).min(64);
    let inner_w = width.saturating_sub(6) as usize;
    let mut lines: Vec<Line> = theme::wrap(question, inner_w)
        .into_iter()
        .map(|l| Line::styled(l, Style::new().fg(theme::TEXT).bold()))
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        theme::pill("y", theme::GREEN),
        Span::styled(" confirm   ", Style::new().fg(theme::SUBTEXT)),
        theme::key("any key"),
        Span::styled(" cancel", Style::new().fg(theme::SUBTEXT)),
    ]));
    let area = theme::centered(screen(f), width, lines.len() as u16 + 4);
    let inner = theme::popup(
        f,
        area,
        theme::YELLOW,
        theme::popup_title("?", "confirm", theme::YELLOW),
    );
    let [body] = Layout::vertical([Constraint::Fill(1)])
        .vertical_margin(1)
        .areas(inner);
    f.render_widget(Paragraph::new(lines), body);
}

fn field_label(form: &NewBeadForm, i: usize, name: &str) -> Span<'static> {
    let text = format!("{name:<13}");
    if form.field == i {
        Span::styled(format!("▸ {text}"), Style::new().fg(theme::GREEN).bold())
    } else {
        Span::styled(format!("  {text}"), Style::new().fg(theme::OVERLAY))
    }
}

fn choice(name: String, on: bool, focused: bool) -> Span<'static> {
    match (on, focused) {
        (true, true) => theme::pill(name, theme::GREEN),
        (true, false) => theme::soft_pill(name, theme::TEXT),
        (false, _) => Span::styled(format!(" {name} "), Style::new().fg(theme::MUTED)),
    }
}

fn choices(form: &NewBeadForm, i: usize, name: &str, opts: Vec<(String, bool)>) -> Line<'static> {
    let mut spans = vec![field_label(form, i, name)];
    for (n, on) in opts {
        spans.push(choice(n, on, form.field == i));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn text_field(
    form: &NewBeadForm,
    i: usize,
    name: &str,
    value: &str,
    placeholder: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let focused = form.field == i;
    let avail = width.saturating_sub(15).max(8);
    let style = Style::new().fg(theme::TEXT);
    let mut rows: Vec<String> = if value.is_empty() {
        vec![String::new()]
    } else {
        value
            .split('\n')
            .flat_map(|l| {
                let chars: Vec<char> = l.chars().collect();
                if chars.is_empty() {
                    vec![String::new()]
                } else {
                    chars.chunks(avail).map(|c| c.iter().collect()).collect()
                }
            })
            .collect()
    };
    if focused && rows.last().is_some_and(|r| r.chars().count() == avail) {
        rows.push(String::new());
    }
    let last = rows.len() - 1;
    rows.into_iter()
        .enumerate()
        .map(|(j, r)| {
            let mut spans = vec![if j == 0 {
                field_label(form, i, name)
            } else {
                Span::raw(" ".repeat(15))
            }];
            if value.is_empty() && !focused {
                spans.push(Span::styled(
                    placeholder.to_string(),
                    Style::new().fg(theme::MUTED).italic(),
                ));
            } else {
                spans.push(Span::styled(r, style));
            }
            if focused && j == last {
                spans.push(Span::styled("▏", Style::new().fg(theme::GREEN)));
            }
            Line::from(spans)
        })
        .collect()
}

fn new_bead(f: &mut Frame, form: &NewBeadForm) {
    let width = f.area().width.saturating_sub(4).min(78);
    let inner_w = width.saturating_sub(6) as usize;
    let prio = (0..=4u8)
        .map(|p| {
            let on = p == form.priority;
            let focused = form.field == 3;
            if on && focused {
                theme::pill(format!("P{p}"), theme::priority(p))
            } else if on {
                Span::styled(
                    format!(" P{p} "),
                    Style::new()
                        .fg(theme::priority(p))
                        .bg(theme::SURFACE)
                        .bold(),
                )
            } else {
                Span::styled(format!(" P{p} "), Style::new().fg(theme::MUTED))
            }
        })
        .flat_map(|s| [s, Span::raw(" ")]);
    let mut prio_line = vec![field_label(form, 3, "Priority")];
    prio_line.extend(prio);
    let mut agents = vec![("default".to_string(), form.agent.is_none())];
    agents.extend(
        AgentKind::ALL
            .into_iter()
            .map(|k| (k.name().to_string(), form.agent == Some(k))),
    );
    let mut presets = vec![("none".to_string(), form.preset.is_none())];
    presets.extend(
        form.presets
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), form.preset == Some(i))),
    );
    let starts = vec![
        ("Pending".to_string(), !form.backlog),
        ("Backlog".to_string(), form.backlog),
    ];
    let mut lines = text_field(form, 0, "Title", &form.title, "required", inner_w);
    lines.push(Line::raw(""));
    lines.extend(text_field(
        form,
        1,
        "Description",
        &form.description,
        "optional",
        inner_w,
    ));
    lines.push(Line::raw(""));
    lines.extend(text_field(
        form,
        2,
        "Acceptance",
        &form.acceptance,
        "optional",
        inner_w,
    ));
    lines.push(Line::raw(""));
    lines.push(Line::from(prio_line));
    lines.push(Line::raw(""));
    lines.push(choices(form, 4, "Agent", agents));
    lines.push(Line::raw(""));
    lines.push(choices(form, 5, "Preset", presets));
    lines.push(Line::raw(""));
    lines.push(choices(form, 6, "Start in", starts));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "─".repeat(inner_w),
        Style::new().fg(theme::LINE),
    ));
    lines.push(Line::from(theme::hint(&[
        ("Tab", "field"),
        ("←→", "choose"),
        ("0-4", "priority"),
        ("Enter", "create"),
        ("Esc", "cancel"),
    ])));
    let rows: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner_w.max(1)).max(1))
        .sum();
    let area = theme::centered(screen(f), width, rows as u16 + 4);
    let inner = theme::popup(
        f,
        area,
        theme::GREEN,
        theme::popup_title("+", "new bead", theme::GREEN),
    );
    let [body] = Layout::vertical([Constraint::Fill(1)])
        .vertical_margin(1)
        .areas(inner);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

const COMMENT_PREVIEW_LINES: usize = 8;

fn comment(f: &mut Frame, d: &Diff, (start, end): (usize, usize), buf: &str, edit: bool) {
    let Some(file) = d.files.get(d.sel) else {
        return;
    };
    let (Some(first), Some(last)) = (file.lines.get(start), file.lines.get(end)) else {
        return;
    };
    let width = f.area().width.saturating_sub(4).min(80);
    let inner_w = width.saturating_sub(6) as usize;
    let span = &file.lines[start..=end];
    let mut lines: Vec<Line> = Vec::new();
    for l in span.iter().take(COMMENT_PREVIEW_LINES) {
        let text = l.text.replace('\t', "    ");
        let (fg, bg) = match l.kind {
            review::Kind::Add => (theme::GREEN, theme::ADD_BG),
            review::Kind::Del => (theme::RED, theme::DEL_BG),
            review::Kind::Hunk => (theme::SKY, theme::HUNK_BG),
            _ => (theme::SUBTEXT, theme::RAISED),
        };
        for chunk in text
            .chars()
            .collect::<Vec<_>>()
            .chunks(inner_w.max(1))
            .map(|c| c.iter().collect::<String>())
            .chain(text.is_empty().then(String::new))
        {
            let pad = inner_w.saturating_sub(chunk.chars().count());
            lines.push(Line::styled(
                format!("{chunk}{}", " ".repeat(pad)),
                Style::new().fg(fg).bg(bg),
            ));
        }
    }
    if span.len() > COMMENT_PREVIEW_LINES {
        lines.push(Line::styled(
            format!("… {} more", span.len() - COMMENT_PREVIEW_LINES),
            Style::new().fg(theme::OVERLAY).italic(),
        ));
    }
    lines.push(Line::raw(""));
    let typed: Vec<String> = theme::wrap(buf, inner_w.saturating_sub(3));
    let last_row = typed.len() - 1;
    for (j, t) in typed.into_iter().enumerate() {
        let mut spans = vec![
            Span::styled(
                if j == 0 { "› " } else { "  " },
                Style::new().fg(theme::MAUVE).bold(),
            ),
            Span::styled(t, Style::new().fg(theme::TEXT)),
        ];
        if j == last_row {
            spans.push(Span::styled("▏", Style::new().fg(theme::MAUVE)));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(theme::hint(&[
        ("Enter", if edit { "save" } else { "add" }),
        ("Ctrl-u", "clear"),
        ("Esc", "cancel"),
    ])));
    let num = |l: &DLine| match (l.new, l.old) {
        (Some(n), _) => Some(n.to_string()),
        (None, Some(o)) => Some(format!("{o} (old)")),
        _ => None,
    };
    let loc = match (num(first), num(last)) {
        (Some(a), Some(b)) if start != end => format!("{}:{a}-{b}", file.name),
        (Some(a), _) => format!("{}:{a}", file.name),
        _ => file.name.clone(),
    };
    let area = theme::centered(f.area(), width, lines.len() as u16 + 4);
    let verb = if edit { "edit" } else { "comment" };
    let inner = theme::popup(
        f,
        area,
        theme::MAUVE,
        theme::popup_title("✎", &format!("{verb} · {loc}"), theme::MAUVE),
    );
    let [body] = Layout::vertical([Constraint::Fill(1)])
        .vertical_margin(1)
        .areas(inner);
    f.render_widget(Paragraph::new(lines), body);
}
