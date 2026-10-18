pub mod board;
pub mod keys;
pub mod progress;
pub mod review;

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, InputKind, Mode, NewBeadForm, View};

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
        Mode::Input { kind, buf } => {
            let prompt = match kind {
                InputKind::Command => ":".to_string(),
                InputKind::Feedback(id) => format!("feedback to {id}: "),
                InputKind::Comment => "comment: ".to_string(),
            };
            vec![Span::raw(format!("{prompt}{buf}")), Span::raw("█")]
        }
        Mode::NewBead(_) => vec![
            tag("NEW", Color::Green),
            Span::raw(" Tab next field, Enter create, Esc cancel").dim(),
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
        "command (:q :q! :new <title> :max <n> :refresh :kill <id>)",
    ),
    ("Board", ""),
    ("h j k l / gg G", "move"),
    ("Enter / p / r", "open / progress / review"),
    ("n", "new bead popup (title, description, priority)"),
    ("+ -", "raise / lower priority"),
    ("X", "kill agent, hold bead"),
    ("D", "discard worktree + branch, hold bead"),
    ("s", "release held bead"),
    ("R", "mark ready for review"),
    ("A", "launch / resume agent"),
    ("q", "quit"),
    ("Progress", ""),
    ("i / a", "insert mode (type into agent)"),
    ("Esc Esc / Ctrl-]", "leave insert mode"),
    ("j k / Ctrl-u Ctrl-d / gg G", "scroll"),
    ("X R r A", "kill / mark review / review / relaunch"),
    ("Review", ""),
    ("j k / Ctrl-d Ctrl-u / gg G", "move line cursor"),
    ("Tab J / S-Tab K / ]f [f", "next / prev file"),
    ("c / x", "comment on line / delete its comments"),
    ("S", "send all comments to agent"),
    ("m", "rebase + fast-forward merge"),
    ("s", "send feedback to agent"),
    ("D", "discard worktree + branch, hold bead"),
    ("p / R", "progress / reload diff"),
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

fn new_bead(f: &mut Frame, form: &NewBeadForm) {
    let [area] = Layout::vertical([Constraint::Length(9)])
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
        Line::from("Tab/S-Tab field  Left/Right or 0-4 priority  Enter create  Esc cancel").dim(),
    ];
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" new bead (description and priority optional) ")),
        area,
    );
}
