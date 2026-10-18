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
            review::render(f, main, d, title)
        }
    }
    status_line(f, status, app);
    if matches!(app.mode, Mode::Help) {
        help(f, app);
    }
    if let Mode::NewBead(form) = &app.mode {
        new_bead(f, form);
    }
    if let Mode::Inspect { id, text, scroll } = &app.mode {
        inspect(f, id, text, *scroll);
    }
    if let (
        Mode::Input {
            kind: InputKind::Comment,
            buf,
        },
        View::Review(d),
    ) = (&app.mode, &app.view)
    {
        comment(f, d, buf);
    }
}

fn status_line(f: &mut Frame, area: Rect, app: &App) {
    let tag = |s: &str, c: Color| {
        Span::styled(format!(" {s} "), Style::new().bg(c).fg(Color::Black).bold())
    };
    let left: Vec<Span> = match &app.mode {
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
        Mode::Input { kind, buf } => {
            let prompt = match kind {
                InputKind::Command => ":".to_string(),
                InputKind::Feedback(id) => format!("feedback to {id}: "),
                InputKind::Comment => String::new(),
            };
            vec![Span::raw(format!("{prompt}{buf}")), Span::raw("█")]
        }
        Mode::NewBead(_) => vec![
            tag("NEW", Color::Green),
            Span::raw(" Tab next field, Enter create, Esc cancel").dim(),
        ],
        Mode::Inspect { .. } => vec![
            tag("INSPECT", Color::Magenta),
            Span::raw(" j k / Ctrl-d Ctrl-u / gg G scroll, p progress, r review, q Esc close")
                .dim(),
        ],
        Mode::Confirm { msg, .. } => {
            vec![tag("CONFIRM", Color::Yellow), Span::raw(format!(" {msg}"))]
        }
    };
    let right = format!(
        "{} {}/{}  base:{}  ? help ",
        app.backend_name(),
        app.running_count(),
        app.cfg.max_agents,
        app.base
    );
    let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(right.len() as u16)])
        .areas(area);
    f.render_widget(Line::from(left), l);
    f.render_widget(Line::from(right).dim(), r);
}

const HELP: &[(&str, &str)] = &[
    ("Global", ""),
    ("?", "help"),
    (
        ":",
        "command (:q :q! :new <title> :max <n> :refresh :kill <id> :chat [new])",
    ),
    ("Board", ""),
    ("h j k l / gg G", "move"),
    ("Enter / p / r", "open / progress / review"),
    ("i", "inspect bead (description, labels, deps)"),
    ("e", "open $EDITOR in bead worktree"),
    (
        "n",
        "new bead popup (title, description, priority, start state)",
    ),
    ("c / C", "open conversation agent / start a fresh one"),
    ("+ -", "raise / lower priority"),
    ("X", "kill agent, hold bead"),
    ("D", "discard worktree + branch, hold bead"),
    ("s", "release held or backlog bead to pending"),
    ("B", "move bead to backlog (not auto scheduled)"),
    ("R", "mark ready for review"),
    ("A", "launch / resume agent"),
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
    ("S", "send all comments to agent"),
    ("m", "rebase + fast-forward merge"),
    ("s", "send feedback to agent"),
    ("D", "discard worktree + branch, hold bead"),
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
    let rows = wrapped(&form.title) + wrapped(&form.description) + 7;
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
    let mut start_line = vec![label(3, "Start in")];
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
        Line::from(start_line),
        Line::raw(""),
        Line::from("Tab/S-Tab field  Left/Right or 0-4 priority  Left/Right start  Enter create  Esc cancel").dim(),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" new bead (only title required) ")),
        area,
    );
}

fn comment(f: &mut Frame, d: &Diff, buf: &str) {
    let Some((file, line)) = d.comment_target() else {
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
        Line::from("Enter add  Ctrl-u clear  Esc cancel").dim(),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(format!(" comment on {loc} "))),
        area,
    );
}
