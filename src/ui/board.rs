use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState};

use crate::agent;
use crate::app::App;
use crate::beads::{Bead, Column, State};
use crate::checks::Check;
use crate::ui::review;

fn badge(app: &App, b: &Bead) -> Span<'static> {
    app.ai_reviews
        .get(&b.id)
        .map_or(Span::raw(""), review::verdict_badge)
}

fn conflict(app: &App, b: &Bead) -> Span<'static> {
    if app.conflicts.contains_key(&b.id) {
        Span::styled("⚠ ", Style::new().fg(Color::Red))
    } else {
        Span::raw("")
    }
}

fn check_badge(app: &App, b: &Bead) -> Span<'static> {
    app.checks.get(&b.id).map_or(Span::raw(""), Check::badge)
}

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
        None if b.status == "blocked" || app.blocked.contains_key(&b.id) => {
            Span::styled("⊘ ", Style::new().fg(Color::Red))
        }
        None => Span::raw("  "),
    }
}

fn count(app: &App, shown: usize, total: usize) -> String {
    if app.filter.is_active() {
        format!("{shown}/{total}")
    } else {
        shown.to_string()
    }
}

fn headline(app: &App, b: &Bead, base: Style) -> Line<'static> {
    let mut spans = vec![
        marker(app, b),
        conflict(app, b),
        badge(app, b),
        check_badge(app, b),
        Span::styled(format!("P{} ", b.priority), Style::new().fg(Color::Magenta)),
    ];
    spans.extend(highlighted(
        &b.title,
        &app.filter.highlights(&b.title),
        base,
    ));
    spans.push(Span::raw(format!(" {}", b.id)).dim());
    Line::from(spans)
}

fn highlighted(text: &str, marks: &[bool], base: Style) -> Vec<Span<'static>> {
    let hit = base.fg(Color::Black).bg(Color::Yellow);
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut on = false;
    for (c, &m) in text.chars().zip(marks) {
        if m != on && !run.is_empty() {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                if on { hit } else { base },
            ));
        }
        on = m;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if on { hit } else { base }));
    }
    spans
}

fn elapsed(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{}h", s / 86400, s % 86400 / 3600),
    }
}

fn activity(app: &App, b: &Bead, now: DateTime<Utc>) -> Line<'static> {
    let mut spans = vec![Span::raw("    ")];
    if let Some(t) = b.state_since() {
        spans.push(Span::styled(
            elapsed((now - t).num_seconds()),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(d) = app.diffstats.get(&b.id) {
        spans.extend([
            Span::styled(format!(" +{}", d.adds), Style::new().fg(Color::Green)),
            Span::styled(format!(" -{}", d.dels), Style::new().fg(Color::Red)),
            Span::raw(format!(" {}f", d.files)).dim(),
        ]);
    }
    let id = if b.state() == Some(State::Reviewing) {
        agent::review_id(&b.id)
    } else {
        b.id.clone()
    };
    if let Some(l) = app.agents.get(&id).and_then(|a| a.last_line()) {
        spans.push(Span::raw(format!(" · {l}")).dim());
    }
    Line::from(spans)
}

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    if app.zoom {
        return render_zoomed(f, area, app);
    }
    let cols = app.columns();
    let totals = app.all_columns().map(|c| c.len());
    let now = Utc::now();
    let areas = Layout::horizontal([Constraint::Fill(1); Column::ALL.len()]).split(area);
    for (i, (beads, col)) in cols.iter().zip(Column::ALL).enumerate() {
        let count = count(app, beads.len(), totals[i]);
        let active = i == app.col;
        let items: Vec<ListItem> = beads
            .iter()
            .map(|b| ListItem::new(vec![headline(app, b, Style::new()), activity(app, b, now)]))
            .collect();
        let border = if active {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new().dim()
        };
        let block = Block::bordered()
            .border_style(border)
            .title(format!(" {} ({count}) ", col.title()));
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
    let mut lines = vec![headline(app, b, Style::new().bold())];
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
    if let Some(c) = app.conflicts.get(&b.id) {
        let files: Vec<&str> = c.files().into_iter().collect();
        lines.push(
            Line::from(format!(
                "    ⚠ {}: {}",
                c.summary(&app.base),
                files.join(", ")
            ))
            .fg(Color::Red),
        );
    }
    lines.push(activity(app, b, Utc::now()));
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
    let totals = app.all_columns().map(|c| c.len());
    let i = app.col;
    let col = Column::ALL[i];
    let beads = &cols[i];
    let width = area.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = beads.iter().map(|b| card(app, b, width)).collect();
    let nav: Vec<Span> = Column::ALL
        .iter()
        .enumerate()
        .flat_map(|(j, c)| {
            let label = Span::raw(format!(
                " {} ({}) ",
                c.title(),
                count(app, cols[j].len(), totals[j])
            ));
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
            count(app, beads.len(), totals[i])
        ))
        .title(nav);
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().bg(Color::DarkGray));
    let mut state = ListState::default().with_selected((!beads.is_empty()).then_some(app.rows[i]));
    f.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::elapsed;

    #[test]
    fn elapsed_is_compact() {
        assert_eq!(elapsed(-5), "0s");
        assert_eq!(elapsed(59), "59s");
        assert_eq!(elapsed(125), "2m");
        assert_eq!(elapsed(3 * 3600 + 5 * 60), "3h05m");
        assert_eq!(elapsed(2 * 86400 + 7 * 3600), "2d7h");
    }
}
