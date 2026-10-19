pub mod board;
pub mod keys;
pub mod progress;
pub mod review;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{App, InputKind, Mode, NewBeadForm, View};
use crate::config::AgentKind;
use crate::health::Status;
use review::Diff;

pub fn draw(f: &mut Frame, app: &App) {
    let [main, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(f.area());
    match &app.view {
        View::Board => board::render(f, main, app),
        View::Progress(id) => progress::render(f, main, app, id),
        View::Review(d) => {
            let title = app
                .all
                .iter()
                .find(|b| b.id == d.id)
                .map(|b| b.title.as_str())
                .unwrap_or("");
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
                },
            )
        }
    }
    status_line(f, status, app);
    if matches!(app.mode, Mode::Help) {
        help(f, app);
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
    if let (Mode::Input { kind, buf }, View::Review(d)) = (&app.mode, &app.view) {
        match kind {
            InputKind::Comment => comment(f, d, buf, false),
            InputKind::EditComment(_) => comment(f, d, buf, true),
            _ => {}
        }
    }
}

fn status_line(f: &mut Frame, area: Rect, app: &App) {
    let tag = |s: &str, c: Color| {
        Span::styled(format!(" {s} "), Style::new().bg(c).fg(Color::Black).bold())
    };
    let left: Vec<Span> = match &app.mode {
        Mode::Health => vec![
            tag("HEALTH", Color::Cyan),
            Span::raw(" r recheck, any other key closes").dim(),
        ],
        Mode::Normal | Mode::Help => vec![
            tag("NORMAL", Color::Blue),
            Span::raw(format!(" {}", app.status)),
        ],
        Mode::Insert => vec![
            tag("INSERT", Color::Green),
            Span::raw(" Esc Esc or Ctrl-] to leave").dim(),
        ],
        Mode::Input {
            kind: InputKind::Comment,
            ..
        } => vec![
            tag("COMMENT", Color::Magenta),
            Span::raw(" Enter add, Ctrl-u clear, Esc cancel").dim(),
        ],
        Mode::Input {
            kind: InputKind::EditComment(_),
            ..
        } => vec![
            tag("EDIT", Color::Magenta),
            Span::raw(" Enter save, Ctrl-u clear, Esc cancel").dim(),
        ],
        Mode::Input { kind, buf } => {
            let prompt = match kind {
                InputKind::Command => ":".to_string(),
                InputKind::Search(_) => "/".to_string(),
                InputKind::Feedback(id) => format!("feedback to {id}: "),
                InputKind::Retry(id) => format!("retry {id}, why did it fail: "),
                InputKind::Comment | InputKind::EditComment(_) => String::new(),
            };
            vec![Span::raw(format!("{prompt}{buf}")), Span::raw("█")]
        }
        Mode::NewBead(_) => vec![
            tag("NEW", Color::Green),
            Span::raw(" Tab next field, Enter create, Esc cancel").dim(),
        ],
        Mode::Inspect { .. } => vec![
            tag("INSPECT", Color::Magenta),
            Span::raw(" j k / Ctrl-d Ctrl-u / gg G scroll, p progress, r review, l session log, h timeline, q Esc close")
                .dim(),
        ],
        Mode::Confirm { msg, .. } => {
            vec![tag("CONFIRM", Color::Yellow), Span::raw(format!(" {msg}"))]
        }
    };
    let usage = if app.run_usage.is_empty() {
        String::new()
    } else {
        format!("{}  ", app.run_usage.short())
    };
    let right = format!(
        "{usage}{}/{} {}/{}  base:{}  ? help ",
        app.backend_name(),
        app.sandbox_name(),
        app.running_count(),
        app.cfg.max_agents,
        app.base
    );
    let mut spans = Vec::new();
    if app.filter.is_active() {
        spans.push(tag("FILTER", Color::Yellow));
        spans.push(Span::raw(format!(" /{} ", app.filter.query)));
    }
    if app.paused {
        spans.push(tag("PAUSED", Color::Yellow));
        spans.push(Span::raw(" "));
    }
    if let Some(d) = app.backoff.remaining(std::time::Instant::now()) {
        spans.push(tag(
            &format!("RATE LIMITED {}", crate::scheduler::fmt_duration(d)),
            Color::Red,
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::raw(right).dim());
    let width: usize = spans.iter().map(|s| s.width()).sum();
    let [l, r] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(width as u16)]).areas(area);
    f.render_widget(Line::from(left), l);
    f.render_widget(Line::from(spans), r);
}

const HELP: &[(&str, &str)] = &[
    ("Global", ""),
    ("?", "help"),
    (
        "! / :health",
        "health of bd, agent, kache, incus and why nothing starts",
    ),
    (
        ":",
        "command (:q :q! :new <title> :max <n> :pause :resume :refresh :kill <id> :chat [new] :health)",
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
    (
        "n",
        "new bead popup (title, description, priority, agent, preset, start state)",
    ),
    ("c / C", "open conversation agent / start a fresh one"),
    ("+ -", "raise / lower priority"),
    ("X", "kill agent and hold bead, or stop ai reviewer"),
    ("D", "discard worktree + branch, hold bead"),
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
    ("q", "quit"),
    ("Progress", ""),
    ("i / a", "insert mode (type into agent, resumes if exited)"),
    ("Esc Esc / Ctrl-]", "leave insert mode"),
    ("j k / Ctrl-u Ctrl-d / gg G", "scroll"),
    ("X R r A", "kill / mark review / review / relaunch"),
    ("e", "open $EDITOR in bead worktree"),
    ("q / Esc", "back to review if opened from it, else board"),
    ("Review", ""),
    ("j k / Ctrl-d Ctrl-u / gg G", "move line cursor"),
    ("Tab J / S-Tab K / ]f [f", "next / prev file"),
    ("c / x", "comment on line / delete its comments"),
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
    ("t / T", "show pre-merge check output / rerun checks"),
    ("V", "start ai reviewer, its comments show inline"),
    ("s", "send feedback to agent"),
    ("D", "discard worktree + branch, hold bead"),
    ("F", "retry from scratch with a note for the agent"),
    ("p / a", "swap to agent view (keeps comments, r to return)"),
    ("i", "swap to agent view in insert mode"),
    ("R", "reload diff"),
    ("e", "open $EDITOR in bead worktree, reloads diff after"),
];

fn help(f: &mut Frame, _app: &App) {
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, d)| {
            if d.is_empty() {
                Line::from(Span::raw(*k).bold().yellow())
            } else {
                Line::from(vec![Span::raw(format!("  {k:<28}")).cyan(), Span::raw(*d)])
            }
        })
        .collect();
    let [area] = Layout::vertical([Constraint::Length(lines.len() as u16 + 2)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::horizontal([Constraint::Length(80)])
        .flex(Flex::Center)
        .areas(area);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" sulla keys ")),
        area,
    );
}

fn health(f: &mut Frame, app: &App) {
    let mut lines = vec![Line::from(Span::raw("Tools").bold().yellow())];
    match &app.health {
        None => lines.push(Line::raw("  checking...").dim()),
        Some(tools) => {
            for t in tools {
                let (mark, color, text) = match &t.status {
                    Status::Ok(v) => ("ok", Color::Green, v.clone()),
                    Status::Missing => (
                        "missing",
                        Color::Red,
                        format!("`{}` not found on PATH", t.bin),
                    ),
                    Status::Failed(e) => ("error", Color::Red, e.clone()),
                    Status::Off(why) => ("off", Color::DarkGray, why.clone()),
                };
                lines.push(Line::from(vec![
                    Span::raw(format!("  {:<10}", t.name)).cyan(),
                    Span::styled(format!("{mark:<8}"), Style::new().fg(color).bold()),
                    Span::raw(text),
                ]));
            }
        }
    }
    if let Some(e) = &app.bd_error {
        lines.push(Line::from(vec![
            Span::raw(format!("  {:<10}", "bd poll")).cyan(),
            Span::styled(
                format!("{:<8}", "error"),
                Style::new().fg(Color::Red).bold(),
            ),
            Span::raw(e.clone()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::raw("Scheduler").bold().yellow()));
    lines.push(Line::raw(format!(
        "  {} of {} agents running",
        app.running_count(),
        app.cfg.max_agents
    )));
    lines.push(match app.idle() {
        None => Line::from(Span::raw("  starting work").green()),
        Some(i) => Line::from(vec![
            Span::raw("  idle: ").red().bold(),
            Span::raw(i.describe()),
        ]),
    });
    let rows: usize = lines.iter().map(|l| l.width().div_ceil(88).max(1)).sum();
    let height = (rows as u16 + 2).min(f.area().height);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::horizontal([Constraint::Length(90)])
        .flex(Flex::Center)
        .areas(area);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" health ")),
        area,
    );
}

fn inspect(f: &mut Frame, id: &str, text: &str, scroll: u16) {
    let lines: Vec<Line> = text.lines().map(|l| Line::raw(l.trim_end())).collect();
    let [area] = Layout::vertical([Constraint::Length(lines.len() as u16 + 2)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::horizontal([Constraint::Length(90)])
        .flex(Flex::Center)
        .areas(area);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(Block::bordered().title(format!(" inspect {id} "))),
        area,
    );
}

fn new_bead(f: &mut Frame, form: &NewBeadForm) {
    let inner_width = 62usize;
    let wrapped = |text: &str| (13 + text.chars().count() + 1).div_ceil(inner_width).max(1);
    let preset_width: usize = 19
        + form
            .presets
            .iter()
            .map(|p| p.chars().count() + 2)
            .sum::<usize>();
    let rows = wrapped(&form.title)
        + wrapped(&form.description)
        + preset_width.div_ceil(inner_width).max(1)
        + 10;
    let height = (rows as u16 + 2).min(f.area().height);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::horizontal([Constraint::Length(64)])
        .flex(Flex::Center)
        .areas(area);
    let label = |i: usize, name: &str| {
        let s = Span::raw(format!("{name:<13}"));
        if form.field == i {
            s.bold().yellow()
        } else {
            s.dim()
        }
    };
    let cursor = |i: usize| if form.field == i { "█" } else { "" };
    let prio = (0..=4)
        .map(|p| {
            let s = Span::raw(format!(" P{p} "));
            if p == form.priority {
                s.black().on_cyan()
            } else {
                s.dim()
            }
        })
        .collect::<Vec<_>>();
    let mut prio_line = vec![label(2, "Priority")];
    prio_line.extend(prio);
    let choice = |name: String, on: bool| {
        let s = Span::raw(format!(" {name} "));
        if on { s.black().on_cyan() } else { s.dim() }
    };
    let mut agent_line = vec![label(3, "Agent")];
    agent_line.push(choice("default".into(), form.agent.is_none()));
    agent_line.extend(
        AgentKind::ALL
            .into_iter()
            .map(|k| choice(k.name().into(), form.agent == Some(k))),
    );
    let mut preset_line = vec![label(4, "Preset")];
    preset_line.push(choice("none".into(), form.preset.is_none()));
    preset_line.extend(
        form.presets
            .iter()
            .enumerate()
            .map(|(i, n)| choice(n.clone(), form.preset == Some(i))),
    );
    let start = [("Pending", false), ("Backlog", true)]
        .into_iter()
        .map(|(n, v)| {
            let s = Span::raw(format!(" {n} "));
            if v == form.backlog {
                s.black().on_cyan()
            } else {
                s.dim()
            }
        })
        .collect::<Vec<_>>();
    let mut start_line = vec![label(5, "Start in")];
    start_line.extend(start);
    let lines = vec![
        Line::from(vec![
            label(0, "Title"),
            Span::raw(format!("{}{}", form.title, cursor(0))),
        ]),
        Line::raw(""),
        Line::from(vec![
            label(1, "Description"),
            Span::raw(format!("{}{}", form.description, cursor(1))),
        ]),
        Line::raw(""),
        Line::from(prio_line),
        Line::raw(""),
        Line::from(agent_line),
        Line::raw(""),
        Line::from(preset_line),
        Line::raw(""),
        Line::from(start_line),
        Line::raw(""),
        Line::from("Tab/S-Tab field  Left/Right choose  0-4 priority  Enter create  Esc cancel")
            .dim(),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" new bead (only title required) ")),
        area,
    );
}

fn comment(f: &mut Frame, d: &Diff, buf: &str, edit: bool) {
    let Some(file) = d.files.get(d.sel) else {
        return;
    };
    let Some(line) = file.lines.get(file.cursor) else {
        return;
    };
    let inner_width = 62usize;
    let wrapped = |text: &str| text.chars().count().div_ceil(inner_width).max(1);
    let code = line.text.replace('\t', "    ");
    let input = format!("{buf}█");
    let rows = wrapped(&code) + wrapped(&input) + 4;
    let height = (rows as u16 + 2).min(f.area().height);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::horizontal([Constraint::Length(64)])
        .flex(Flex::Center)
        .areas(area);
    let loc = match (line.new, line.old) {
        (Some(n), _) => format!("{}:{n}", file.name),
        (None, Some(o)) => format!("{}:{o} (old)", file.name),
        _ => file.name.clone(),
    };
    let lines = vec![
        Line::from(code).dim(),
        Line::raw(""),
        Line::from(input),
        Line::raw(""),
        Line::from(if edit {
            "Enter save  Ctrl-u clear  Esc cancel"
        } else {
            "Enter add  Ctrl-u clear  Esc cancel"
        })
        .dim(),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" comment on {loc} "))),
        area,
    );
}
