use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Mode};

pub fn render(f: &mut Frame, area: Rect, app: &App, id: &str) {
    let title = app
        .all
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.title.as_str())
        .unwrap_or("");
    let mut spans = vec![
        Span::raw(format!(" {id} ")).bold(),
        Span::raw(format!("{title} ")),
    ];
    if app.parked_review.as_ref().is_some_and(|d| d.id == id) {
        spans.push(Span::styled(
            "[r back to review] ",
            Style::new().fg(Color::Cyan),
        ));
    }
    let Some(agent) = app.agents.get(id) else {
        let block = Block::bordered().title(Line::from(spans));
        f.render_widget(
            Paragraph::new("No agent attached. Press A to launch or resume one.").block(block),
            area,
        );
        return;
    };
    let insert = matches!(app.mode, Mode::Insert);
    if !agent.alive {
        spans.push(Span::styled(
            "[exited, A to resume] ",
            Style::new().fg(Color::Red),
        ));
    } else if insert {
        spans.push(Span::styled("[INSERT] ", Style::new().fg(Color::Green)));
    }
    let parser = agent.parser.lock().unwrap();
    let screen = parser.screen();
    if screen.scrollback() > 0 {
        spans.push(Span::styled(
            format!("[scroll -{}] ", screen.scrollback()),
            Style::new().fg(Color::Yellow),
        ));
    }
    let border = if insert {
        Style::new().fg(Color::Green)
    } else {
        Style::new()
    };
    let block = Block::bordered()
        .border_style(border)
        .title(Line::from(spans));
    f.render_widget(PseudoTerminal::new(screen).block(block), area);
}
