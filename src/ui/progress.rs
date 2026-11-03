use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_term::widget::PseudoTerminal;

use crate::agent::{self, CHAT_ID};
use crate::app::{App, Mode};
use crate::ui::theme;

pub fn render(f: &mut Frame, area: Rect, app: &App, id: &str) {
    let reviewed = agent::reviewed_bead(id);
    let bead = reviewed.unwrap_or(id);
    let title = app
        .all
        .iter()
        .find(|b| b.id == bead)
        .map(|b| b.title.as_str())
        .unwrap_or("");
    let insert = matches!(app.mode, Mode::Insert);
    let agent = app.agents.get(id);
    let alive = agent.is_some_and(|a| a.alive);
    let accent = if insert {
        theme::GREEN
    } else if reviewed.is_some() {
        theme::SKY
    } else if id == CHAT_ID {
        theme::MAUVE
    } else {
        theme::BLUE
    };
    let mut spans = vec![Span::raw(" ")];
    if id == CHAT_ID {
        spans.push(theme::pill("◆ conversation", accent));
    } else {
        spans.push(theme::pill(format!("◑ {bead}"), accent));
        if reviewed.is_some() {
            spans.push(Span::raw(" "));
            spans.push(theme::soft_pill("ai review", theme::SKY));
        }
        spans.push(Span::styled(
            format!(" {title}"),
            Style::new().fg(theme::TEXT).bold(),
        ));
    }
    spans.push(Span::raw(" "));
    let mut right = vec![Span::raw(" ")];
    if let Some(loc) = app.slot_location(id) {
        right.push(theme::soft_pill(loc, theme::LAVENDER));
        right.push(Span::raw(" "));
    }
    if app.parked_review.as_ref().is_some_and(|d| d.id == id) {
        right.extend(theme::hint(&[("r", "back to review")]));
        right.push(Span::raw(" "));
    }
    match agent {
        None => {}
        Some(_) if !alive => {
            right.push(theme::pill("✗ exited", theme::RED));
            right.push(Span::raw(" "));
        }
        Some(_) if insert => {
            right.push(theme::pill("INSERT", theme::GREEN));
            right.push(Span::raw(" "));
        }
        Some(_) => {
            right.push(Span::styled("● live ", Style::new().fg(theme::GREEN)));
        }
    }
    let block = theme::panel(accent, true)
        .title(Line::from(spans))
        .title(Line::from(right).right_aligned());
    let Some(agent) = agent else {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [row] = Layout::vertical([Constraint::Length(2)])
            .flex(Flex::Center)
            .areas(inner);
        let mut hint = vec![Span::styled(
            "No agent attached.  ",
            Style::new().fg(theme::OVERLAY).italic(),
        )];
        hint.extend(theme::hint(&[("A", "launch or resume")]));
        f.render_widget(Paragraph::new(Line::from(hint)).centered(), row);
        return;
    };
    let parser = agent.parser.lock().unwrap();
    let screen = parser.screen();
    let mut bottom = vec![Span::raw(" ")];
    if screen.scrollback() > 0 {
        bottom.push(theme::pill(
            format!("↑ scrolled {}", screen.scrollback()),
            theme::YELLOW,
        ));
        bottom.push(Span::raw("  "));
        bottom.extend(theme::hint(&[("G", "follow")]));
    } else if !alive {
        bottom.extend(theme::hint(&[("A", "resume")]));
    } else if insert {
        bottom.extend(theme::hint(&[("Esc Esc / Ctrl-]", "leave insert")]));
    } else {
        bottom.extend(theme::hint(&[
            ("i", "type"),
            ("j k", "scroll"),
            ("q", "back"),
        ]));
    }
    bottom.push(Span::raw(" "));
    let block = block.title_bottom(Line::from(bottom));
    f.render_widget(PseudoTerminal::new(screen).block(block), area);
}
