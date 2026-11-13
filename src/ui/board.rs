use std::time::Instant;

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::layout::{Flex, Margin};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::{self, Stall};
use crate::app::App;
use crate::beads::{Bead, Column, State};
use crate::checks::Check;
use crate::pr::Checks;
use crate::scheduler;
use crate::ui::review;
use crate::ui::theme;

fn badge(app: &App, b: &Bead) -> Span<'static> {
    app.ai_reviews
        .get(&b.id)
        .map_or(Span::raw(""), review::verdict_badge)
}

fn released_badge(b: &Bead) -> Span<'static> {
    if b.status != "closed" && b.state() == Some(State::Released) {
        review::released_badge()
    } else {
        Span::raw("")
    }
}

fn stall_marker(s: Stall) -> Span<'static> {
    match s {
        Stall::Waiting => Span::styled("? ", Style::new().fg(theme::YELLOW).bold()),
        Stall::Stalled => Span::styled("⧗ ", Style::new().fg(theme::PEACH)),
        Stall::Dead => Span::styled("✗ ", Style::new().fg(theme::RED).bold()),
    }
}

fn stall_of(app: &App, b: &Bead) -> Option<(String, Stall)> {
    let id = if b.state() == Some(State::Reviewing) {
        agent::review_id(&b.id)
    } else {
        b.id.clone()
    };
    app.stalls.get(&id).map(|s| (id, *s))
}

fn conflict(app: &App, b: &Bead) -> Span<'static> {
    if app.conflicts.contains_key(&b.id) {
        Span::styled("⚠ ", Style::new().fg(theme::RED))
    } else {
        Span::raw("")
    }
}

fn check_badge(app: &App, b: &Bead) -> Span<'static> {
    app.checks.get(&b.id).map_or(Span::raw(""), Check::badge)
}

fn pr_phase(app: &App, b: &Bead) -> Option<Span<'static>> {
    b.label("pr")?;
    app.pr_watch.beads.get(&b.id)?;
    let (text, color) = match b.state()? {
        State::Revise => ("revise queued", theme::PINK),
        State::Running => ("revising", theme::YELLOW),
        State::Reviewing => ("ai reviewing", theme::SKY),
        State::Review => ("revised, Enter to review", theme::GREEN),
        _ => return None,
    };
    Some(Span::styled(text, Style::new().fg(color).bold()))
}

fn pr_badges(app: &App, b: &Bead) -> Vec<Span<'static>> {
    let Some(seen) = app.pr_watch.beads.get(&b.id) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    if !seen.comments.is_empty() {
        spans.push(Span::styled(
            format!("✉{} ", seen.comments.len()),
            Style::new().fg(theme::SKY),
        ));
    }
    if !seen.pending.is_empty() {
        let n = seen.pending.count();
        spans.push(if b.state() == Some(State::Pr) {
            Span::styled(
                format!("↻{n}, U to revise "),
                Style::new().fg(theme::PINK).bold(),
            )
        } else {
            Span::styled(format!("↻{n} "), Style::new().fg(theme::PINK))
        });
    }
    if seen.closed {
        spans.push(Span::styled("closed ", Style::new().fg(theme::RED)));
    }
    spans.push(match seen.checks {
        Checks::None => Span::raw(""),
        Checks::Pending => Span::styled("⟳ci ", Style::new().fg(theme::YELLOW)),
        Checks::Passing => Span::styled("✓ci ", Style::new().fg(theme::GREEN)),
        Checks::Failing(_) => Span::styled("✗ci ", Style::new().fg(theme::RED)),
    });
    spans
}

fn starting(app: &App, b: &Bead) -> bool {
    app.is_starting(&b.id) || app.is_starting(&agent::review_id(&b.id))
}

fn marker(app: &App, b: &Bead) -> Span<'static> {
    if let Some((_, s)) = stall_of(app, b) {
        return stall_marker(s);
    }
    if starting(app, b) {
        return Span::styled("◌ ", Style::new().fg(theme::GREEN));
    }
    if b.state() == Some(State::Reviewing) {
        let color = match app.agents.get(&agent::review_id(&b.id)) {
            Some(a) if a.alive => theme::SKY,
            _ => theme::RED,
        };
        return Span::styled("◎ ", Style::new().fg(color));
    }
    match app.agents.get(&b.id) {
        Some(a) if a.alive => Span::styled("● ", Style::new().fg(theme::GREEN)),
        Some(_) => Span::styled("✗ ", Style::new().fg(theme::RED)),
        None if b.state() == Some(State::Held) => {
            let color = if app.over_budget.contains_key(&b.id) {
                theme::RED
            } else {
                theme::YELLOW
            };
            Span::styled("⏸ ", Style::new().fg(color))
        }
        None if b.status == "blocked" || app.blocked.contains_key(&b.id) => {
            Span::styled("⊘ ", Style::new().fg(theme::RED))
        }
        None => Span::raw("  "),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Relation {
    None,
    Ancestor,
    Descendant,
}

fn relation(chain: Option<&(String, scheduler::Chain)>, b: &Bead) -> Relation {
    match chain {
        Some((_, c)) if c.ancestors.contains(&b.id) => Relation::Ancestor,
        Some((_, c)) if c.descendants.contains(&b.id) => Relation::Descendant,
        _ => Relation::None,
    }
}

fn count(app: &App, shown: usize, total: usize) -> String {
    if app.is_filtered() {
        format!("{shown}/{total}")
    } else {
        shown.to_string()
    }
}

fn headline(app: &App, b: &Bead, base: Style, rel: Relation, width: usize) -> Line<'static> {
    let (glyph, base) = match rel {
        Relation::Ancestor => ("▲", base.fg(theme::YELLOW)),
        Relation::Descendant => ("▼", base.fg(theme::PINK)),
        Relation::None => (" ", base),
    };
    let mut spans = vec![
        Span::styled(glyph, base.bold()),
        marker(app, b),
        conflict(app, b),
        released_badge(b),
        badge(app, b),
        check_badge(app, b),
        Span::styled(
            format!("P{} ", b.priority),
            Style::new().fg(theme::priority(b.priority)).bold(),
        ),
    ];
    if let Some(n) = b.label("pr") {
        spans.push(Span::styled(format!("#{n} "), Style::new().fg(theme::SKY)));
        spans.extend(pr_badges(app, b));
    }
    let id = Span::styled(format!(" {}", b.id), Style::new().fg(theme::MUTED));
    let used: usize = spans.iter().map(Span::width).sum::<usize>() + id.width();
    let (title, cut) = fit(&b.title, width.saturating_sub(used));
    spans.extend(highlighted(title, &app.filter.highlights(&b.title), base));
    if cut {
        spans.push(Span::styled("…", base));
    }
    spans.push(id);
    Line::from(spans)
}

fn fit(text: &str, room: usize) -> (&str, bool) {
    if text.width() <= room {
        return (text, false);
    }
    let budget = room.saturating_sub(1);
    let mut used = 0;
    let mut end = 0;
    for (i, c) in text.char_indices() {
        used += c.width().unwrap_or(0);
        if used > budget {
            break;
        }
        end = i + c.len_utf8();
    }
    (&text[..end], room > 0)
}

fn highlighted(text: &str, marks: &[bool], base: Style) -> Vec<Span<'static>> {
    let hit = base.fg(theme::CRUST).bg(theme::YELLOW);
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

fn app_span(app: &App, b: &Bead) -> Option<Span<'static>> {
    let (live, other) = app.app_status(&b.id)?;
    let (text, color) = theme::app_badge(live, other);
    Some(Span::styled(text, Style::new().fg(color)))
}

fn activity(app: &App, b: &Bead, now: DateTime<Utc>, with_app: bool) -> Line<'static> {
    let mut spans = vec![Span::raw("    ")];
    if let Some(p) = pr_phase(app, b) {
        spans.extend([p, Span::raw(" ")]);
    }
    if let Some(t) = b.state_since() {
        spans.push(Span::styled(
            elapsed((now - t).num_seconds()),
            Style::new().fg(theme::SUBTEXT),
        ));
    }
    if let Some(d) = app.diffstats.get(&b.id) {
        spans.extend([
            Span::styled(format!(" +{}", d.adds), Style::new().fg(theme::GREEN)),
            Span::styled(format!(" -{}", d.dels), Style::new().fg(theme::RED)),
            Span::styled(format!(" {}f", d.files), Style::new().fg(theme::OVERLAY)),
        ]);
    }
    if let Some(a) = app_span(app, b).filter(|_| with_app) {
        spans.extend([Span::raw(" "), a]);
    }
    let id = if b.state() == Some(State::Reviewing) {
        agent::review_id(&b.id)
    } else {
        b.id.clone()
    };
    let quiet = Style::new().fg(theme::MUTED).italic();
    if starting(app, b) {
        spans.extend([theme::dot(), Span::styled("starting", quiet)]);
    } else if let Some(l) = app.agents.get(&id).and_then(|a| a.last_line()) {
        spans.extend([theme::dot(), Span::styled(l, quiet)]);
    }
    Line::from(spans)
}

fn column_title(app: &App, col: Column, shown: usize, total: usize, active: bool) -> Line<'static> {
    let accent = theme::column_accent(col);
    let name = Style::new().fg(if active { accent } else { theme::SUBTEXT });
    let n = count(app, shown, total);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(theme::column_icon(col), Style::new().fg(accent)),
        Span::styled(format!(" {} ", col.title()), name.bold()),
        if active {
            theme::pill(n, accent)
        } else {
            theme::soft_pill(n, theme::OVERLAY)
        },
        Span::raw(" "),
    ])
}

fn empty_hint(f: &mut Frame, area: Rect, app: &App, col: Column) {
    let inner = area.inner(Margin::new(1, 1));
    if inner.height < 2 {
        return;
    }
    let text = if app.is_filtered() {
        "no matches"
    } else {
        theme::column_empty(col)
    };
    let [row] = Layout::vertical([Constraint::Length(2)])
        .flex(Flex::Center)
        .areas(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(theme::column_icon(col), Style::new().fg(theme::LINE)),
            Line::styled(text, Style::new().fg(theme::MUTED).italic()),
        ])
        .centered(),
        row,
    );
}

fn cards(items: Vec<ListItem<'_>>, accent: Color, active: bool) -> List<'_> {
    let list = List::new(items)
        .highlight_spacing(HighlightSpacing::Always)
        .repeat_highlight_symbol(true);
    if active {
        list.highlight_symbol(Span::styled("▌", Style::new().fg(accent)))
            .highlight_style(Style::new().bg(theme::SURFACE).add_modifier(Modifier::BOLD))
    } else {
        list.highlight_symbol(" ")
    }
}

pub fn render(f: &mut Frame, area: Rect, app: &App) {
    if app.zoom {
        return render_zoomed(f, area, app);
    }
    let cols = app.columns();
    let totals = app.all_columns().map(|c| c.len());
    let chain = app.selected_chain();
    let now = Utc::now();
    let shown = app.visible_columns();
    let rects = Layout::horizontal(vec![Constraint::Fill(1); shown.len()]).split(area);
    for (&i, &rect) in shown.iter().zip(rects.iter()) {
        let (beads, col) = (&cols[i], Column::ALL[i]);
        let active = i == app.col;
        let accent = theme::column_accent(col);
        let width = rect.width.saturating_sub(3) as usize;
        let items: Vec<ListItem> = beads
            .iter()
            .map(|b| {
                ListItem::new(vec![
                    headline(app, b, Style::new(), relation(chain.as_ref(), b), width),
                    activity(app, b, now, true),
                ])
            })
            .collect();
        let heights: Vec<usize> = items.iter().map(ListItem::height).collect();
        let block = theme::panel(accent, active).title(column_title(
            app,
            col,
            beads.len(),
            totals[i],
            active,
        ));
        let mut state =
            ListState::default().with_selected((!beads.is_empty()).then_some(app.rows[i]));
        f.render_stateful_widget(cards(items, accent, active).block(block), rect, &mut state);
        if beads.is_empty() {
            empty_hint(f, rect, app, col);
        }
        let hint = Style::new().fg(if active { accent } else { theme::MUTED });
        scroll_hints(f, rect, &heights, state.offset(), hint, true);
    }
}

fn overflow(heights: &[usize], offset: usize, rows: usize) -> (usize, usize) {
    let mut used = 0;
    let mut shown = 0;
    for h in &heights[offset.min(heights.len())..] {
        if used + h > rows {
            break;
        }
        used += h;
        shown += 1;
    }
    let offset = offset.min(heights.len());
    let shown = shown.max(usize::from(offset < heights.len()));
    (offset, heights.len() - offset - shown)
}

fn scroll_hints(
    f: &mut Frame,
    area: Rect,
    heights: &[usize],
    offset: usize,
    style: Style,
    split: bool,
) {
    if area.height < 3 || area.width < 4 {
        return;
    }
    let (above, below) = overflow(heights, offset, area.height as usize - 2);
    let top = Rect::new(area.x + 1, area.y, area.width - 2, 1);
    let bottom = Rect {
        y: area.y + area.height - 1,
        ..top
    };
    let up = (above > 0).then(|| format!("▲ {above} more"));
    let down = (below > 0).then(|| format!("▼ {below} more"));
    let hint = |s: String| Line::styled(format!(" {s} "), style).right_aligned();
    if split {
        if let Some(u) = up {
            f.render_widget(hint(u), top);
        }
        if let Some(d) = down {
            f.render_widget(hint(d), bottom);
        }
    } else {
        let parts: Vec<String> = up.into_iter().chain(down).collect();
        if !parts.is_empty() {
            f.render_widget(hint(parts.join(" · ")), bottom);
        }
    }
}

fn card(app: &App, b: &Bead, width: usize, rel: Relation) -> ListItem<'static> {
    let mut lines = vec![headline(
        app,
        b,
        Style::new().bold(),
        rel,
        width.saturating_sub(1),
    )];
    let mut meta: Vec<Span<'static>> = vec![b.issue_type.clone().into(), b.status.clone().into()];
    let labels: Vec<&str> = b.labels.iter().map(String::as_str).collect();
    if !labels.is_empty() {
        meta.push(labels.join(", ").into());
    }
    if let Some(p) = app.stacked_on.get(&b.id) {
        meta.push(format!("stacked on {p}").into());
    }
    let blockers = scheduler::blockers(b, &app.blocked, &app.stacked_on);
    if !blockers.is_empty() {
        meta.push(format!("blocked by {}", blockers.join(", ")).into());
    }
    if let Some(d) = app.dependents.get(&b.id) {
        if !d.blocks.is_empty() {
            meta.push(format!("blocks {}", d.blocks.join(", ")).into());
        }
        match d.stacked.len() {
            0 => {}
            1 | 2 => meta.push(format!("stacked: {}", d.stacked.join(", ")).into()),
            n => meta.push(format!("{n} stacked").into()),
        }
    }
    if let Some(loc) = app.slot_location(&b.id) {
        meta.push(loc.into());
    }
    if let Some(a) = app_span(app, b) {
        meta.push(a);
    }
    if let Some(u) = app.usage.get(&b.id) {
        meta.push(u.short().into());
    }
    if let Some(budget) = app.budget_status(&b.id) {
        meta.push(budget.into());
    }
    if let Some(m) = app.over_budget.get(&b.id) {
        meta.push(m.clone().into());
    }
    if let Some((id, s)) = stall_of(app, b) {
        let text: String = match s {
            Stall::Waiting => "waiting on input".into(),
            Stall::Stalled => {
                let mins = app
                    .agents
                    .get(&id)
                    .map_or(0, |a| a.idle(Instant::now()).as_secs() / 60);
                format!("stalled, no progress for {mins}m")
            }
            Stall::Dead => "in progress but no agent running".into(),
        };
        meta.push(text.into());
    }
    meta.retain(|m| !m.content.is_empty());
    let mut spans = vec![Span::raw("    ")];
    for (i, m) in meta.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" · "));
        }
        spans.push(m);
    }
    lines.push(Line::from(spans).fg(theme::OVERLAY));
    if let Some(c) = app.conflicts.get(&b.id) {
        let files: Vec<&str> = c.files().into_iter().collect();
        lines.push(
            Line::from(format!(
                "    ⚠ {}: {}",
                c.summary(&app.base),
                files.join(", ")
            ))
            .fg(theme::RED),
        );
    }
    lines.push(activity(app, b, Utc::now(), false));
    let max = width.saturating_sub(4).max(1);
    for l in b
        .description
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .take(3)
    {
        let text: String = l.chars().take(max).collect();
        lines.push(Line::from(format!("    {text}")).fg(theme::MUTED).italic());
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
    let chain = app.selected_chain();
    let items: Vec<ListItem> = beads
        .iter()
        .map(|b| card(app, b, width, relation(chain.as_ref(), b)))
        .collect();
    let heights: Vec<usize> = items.iter().map(ListItem::height).collect();
    let mut nav: Vec<Span> = Vec::new();
    for j in app.visible_columns() {
        let c = &Column::ALL[j];
        let n = count(app, cols[j].len(), totals[j]);
        let accent = theme::column_accent(*c);
        nav.push(if j == i {
            theme::pill(
                format!("{} {} {n}", theme::column_icon(*c), c.title()),
                accent,
            )
        } else {
            Span::styled(
                format!(" {} {n} ", theme::column_icon(*c)),
                Style::new().fg(theme::OVERLAY),
            )
        });
    }
    nav.push(Span::raw(" "));
    let accent = theme::column_accent(col);
    let block = theme::panel(accent, true)
        .title(column_title(app, col, beads.len(), totals[i], true))
        .title(Line::from(nav).right_aligned())
        .title_bottom(
            Line::from(
                [
                    vec![Span::raw(" ")],
                    theme::hint(&[("z / Esc", "unzoom")]),
                    vec![Span::raw(" ")],
                ]
                .concat(),
            )
            .left_aligned(),
        );
    let mut state = ListState::default().with_selected((!beads.is_empty()).then_some(app.rows[i]));
    f.render_stateful_widget(cards(items, accent, true).block(block), area, &mut state);
    if beads.is_empty() {
        empty_hint(f, area, app, col);
    }
    scroll_hints(
        f,
        area,
        &heights,
        state.offset(),
        Style::new().fg(accent),
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::{elapsed, overflow};

    #[test]
    fn overflow_counts_hidden_items() {
        assert_eq!(overflow(&[2, 2, 2], 0, 6), (0, 0));
        assert_eq!(overflow(&[2, 2, 2], 0, 5), (0, 1));
        assert_eq!(overflow(&[2, 2, 2, 2], 1, 4), (1, 1));
        assert_eq!(overflow(&[], 0, 4), (0, 0));
        assert_eq!(overflow(&[9, 2], 0, 4), (0, 1));
    }

    #[test]
    fn elapsed_is_compact() {
        assert_eq!(elapsed(-5), "0s");
        assert_eq!(elapsed(59), "59s");
        assert_eq!(elapsed(125), "2m");
        assert_eq!(elapsed(3 * 3600 + 5 * 60), "3h05m");
        assert_eq!(elapsed(2 * 86400 + 7 * 3600), "2d7h");
    }
}
