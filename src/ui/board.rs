use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState};

use crate::app::App;
use crate::beads::{Bead, Column, State};

fn marker(app: &App, b: &Bead) -> Span<'static> {
    match app.agents.get(&b.id) {
        Some(a) if a.alive => Span::styled("● ", Style::new().fg(Color::Green)),
        Some(_) => Span::styled("✗ ", Style::new().fg(Color::Red)),
        None if b.state() == Some(State::Held) => {
            Span::styled("⏸ ", Style::new().fg(Color::Yellow))
        }
        None if b.status == "blocked" => Span::styled("⊘ ", Style::new().fg(Color::Red)),
        None => Span::raw("  "),
    }
}

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    let cols = app.columns();
    let areas = Layout::horizontal([Constraint::Fill(1); Column::ALL.len()]).split(area);
    for (i, (beads, col)) in cols.iter().zip(Column::ALL).enumerate() {
        let active = i == app.col;
        let items: Vec<ListItem> = beads
            .iter()
            .map(|b| {
                ListItem::new(Line::from(vec![
                    marker(app, b),
                    Span::styled(format!("P{} ", b.priority), Style::new().fg(Color::Magenta)),
                    Span::raw(b.title.clone()),
                    Span::raw(format!(" {}", b.id)).dim(),
                ]))
            })
            .collect();
        let border = if active {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new().dim()
        };
        let block = Block::bordered().border_style(border).title(format!(
            " {} ({}) ",
            col.title(),
            beads.len()
        ));
        let list = List::new(items).block(block).highlight_style(if active {
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        });
        let mut state =
            ListState::default().with_selected((!beads.is_empty()).then_some(app.rows[i]));
        f.render_stateful_widget(list, areas[i], &mut state);
    }
}
