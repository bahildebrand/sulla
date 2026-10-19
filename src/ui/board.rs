use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState};

use crate::agent;
use crate::app::App;
use crate::beads::{Bead, Column, State};

fn marker(app: &App, b: &Bead) -> Span<'static> {
    if b.state() == Some(State::Reviewing) {
        let color = match app.agents.get(&agent::review_id(&b.id)) {
            Some(a) if a.alive => Color::Cyan,
            _ => Color::Red,
        };
        return Span::styled("◎ ", Style::new().fg(color));
    }
    match app.agents.get(&b.id) {
        Some(a) if a.alive => Span::styled("● ", Style::new().fg(Color::Green)),
        Some(_) => Span::styled("✗ ", Style::new().fg(Color::Red)),
        None if b.state() == Some(State::Held) => {
            Span::styled("⏸ ", Style::new().fg(Color::Yellow))
        }
        None if b.status == "blocked" || app.blocked.contains_key(&b.id) => Span::styled("⊘ ", Style::new().fg(Color::Red)),
        None => Span::raw("  "),
    }
}

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    if app.zoom {
        return render_zoomed(f, area, app);
    }
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

fn card(app: &App, b: &Bead, width: usize) -> ListItem<'static> {
    let mut lines = vec![Line::from(vec![
        marker(app, b),
        Span::styled(format!("P{} ", b.priority), Style::new().fg(Color::Magenta)),
        Span::raw(b.title.clone()).bold(),
        Span::raw(format!(" {}", b.id)).dim(),
    ])];
    let mut meta = vec![b.issue_type.clone(), b.status.clone()];
    let labels: Vec<&str> = b.labels.iter().map(String::as_str).collect();
    if !labels.is_empty() {
        meta.push(labels.join(", "));
    }
    let mut blockers = b.open_blockers();
    if let Some(ids) = app.blocked.get(&b.id) {
        for id in ids {
            if !blockers.contains(&id.as_str()) {
                blockers.push(id);
            }
        }
    }
    if !blockers.is_empty() {
        meta.push(format!("blocked by {}", blockers.join(", ")));
    }
    if let Some(loc) = app.slot_location(&b.id) {
        meta.push(loc);
    }
    meta.retain(|m| !m.is_empty());
    lines.push(Line::from(format!("    {}", meta.join(" · "))).fg(Color::Cyan));
    let max = width.saturating_sub(4).max(1);
    for l in b
        .description
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .take(3)
    {
        let text: String = l.chars().take(max).collect();
        lines.push(Line::from(format!("    {text}")).dim());
    }
    lines.push(Line::raw(""));
    ListItem::new(lines)
}

fn render_zoomed(f: &mut Frame, area: Rect, app: &App) {
    let cols = app.columns();
    let i = app.col;
    let col = Column::ALL[i];
    let beads = &cols[i];
    let width = area.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = beads.iter().map(|b| card(app, b, width)).collect();
    let nav: Vec<Span> = Column::ALL
        .iter()
        .enumerate()
        .flat_map(|(j, c)| {
            let label = Span::raw(format!(" {} ({}) ", c.title(), cols[j].len()));
            let label = if j == i {
                label.cyan().bold()
            } else {
                label.dim()
            };
            [label, Span::raw("│").dim()]
        })
        .collect();
    let nav = Line::from(nav[..nav.len() - 1].to_vec()).right_aligned();
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(
            " {} ({}) · zoom, z/Esc to exit ",
            col.title(),
            beads.len()
        ))
        .title(nav);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(Color::DarkGray));
    let mut state = ListState::default().with_selected((!beads.is_empty()).then_some(app.rows[i]));
    f.render_stateful_widget(list, area, &mut state);
}
