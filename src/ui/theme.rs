use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding};

use crate::beads::Column;
use crate::browse::Live;

pub const TEXT: Color = Color::Rgb(205, 214, 244);
pub const SUBTEXT: Color = Color::Rgb(166, 173, 200);
pub const OVERLAY: Color = Color::Rgb(127, 132, 156);
pub const MUTED: Color = Color::Rgb(88, 91, 112);
pub const LINE: Color = Color::Rgb(69, 71, 90);
pub const SURFACE: Color = Color::Rgb(49, 50, 68);
pub const RAISED: Color = Color::Rgb(37, 37, 55);
pub const PANEL: Color = Color::Rgb(24, 24, 37);
pub const CRUST: Color = Color::Rgb(17, 17, 27);
const BACKDROP: (u8, u8, u8) = (30, 30, 46);

pub const RED: Color = Color::Rgb(243, 139, 168);
pub const PEACH: Color = Color::Rgb(250, 179, 135);
pub const YELLOW: Color = Color::Rgb(249, 226, 175);
pub const GREEN: Color = Color::Rgb(166, 227, 161);
pub const TEAL: Color = Color::Rgb(148, 226, 213);
pub const SKY: Color = Color::Rgb(137, 220, 235);
pub const BLUE: Color = Color::Rgb(137, 180, 250);
pub const LAVENDER: Color = Color::Rgb(180, 190, 254);
pub const MAUVE: Color = Color::Rgb(203, 166, 247);
pub const PINK: Color = Color::Rgb(245, 194, 231);
pub const ERROR: Color = RED;

pub const ADD_BG: Color = Color::Rgb(38, 56, 48);
pub const ADD_GUTTER: Color = Color::Rgb(46, 74, 60);
pub const DEL_BG: Color = Color::Rgb(64, 40, 54);
pub const DEL_GUTTER: Color = Color::Rgb(86, 48, 66);
pub const HUNK_BG: Color = Color::Rgb(34, 40, 62);
pub const CURSOR_BG: Color = Color::Rgb(62, 64, 94);
pub const MARK_BG: Color = Color::Rgb(74, 56, 92);

pub fn column_accent(c: Column) -> Color {
    match c {
        Column::Backlog => LAVENDER,
        Column::Pending => YELLOW,
        Column::InProgress => BLUE,
        Column::Review => MAUVE,
        Column::AwaitingPr => SKY,
        Column::Done => GREEN,
    }
}

pub fn column_icon(c: Column) -> &'static str {
    match c {
        Column::Backlog => "○",
        Column::Pending => "◔",
        Column::InProgress => "◑",
        Column::Review => "◕",
        Column::AwaitingPr => "●",
        Column::Done => "✔",
    }
}

pub fn column_empty(c: Column) -> &'static str {
    match c {
        Column::Backlog => "nothing parked",
        Column::Pending => "queue is empty",
        Column::InProgress => "no agents at work",
        Column::Review => "nothing to review",
        Column::AwaitingPr => "no open pull requests",
        Column::Done => "nothing finished yet",
    }
}

pub fn priority(p: u8) -> Color {
    match p {
        0 => RED,
        1 => PEACH,
        2 => YELLOW,
        3 => BLUE,
        _ => OVERLAY,
    }
}

pub fn pill(text: impl Into<String>, bg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::new().fg(CRUST).bg(bg).add_modifier(Modifier::BOLD),
    )
}

pub fn soft_pill(text: impl Into<String>, fg: Color) -> Span<'static> {
    Span::styled(
        format!(" {} ", text.into()),
        Style::new().fg(fg).bg(SURFACE),
    )
}

pub fn app_badge(live: Live, other: Option<&str>) -> (String, Color) {
    match (live, other) {
        (Live::Down, _) => (format!("{} app", live.glyph()), MUTED),
        (_, Some(o)) => (format!("{} app ({o})", live.glyph()), PEACH),
        (Live::Up, None) => (format!("{} app", live.glyph()), GREEN),
        (Live::Partial, None) => (format!("{} app", live.glyph()), YELLOW),
    }
}

pub fn key(k: &str) -> Span<'static> {
    Span::styled(
        format!(" {k} "),
        Style::new()
            .fg(LAVENDER)
            .bg(SURFACE)
            .add_modifier(Modifier::BOLD),
    )
}

pub fn keys(spec: &str) -> Vec<Span<'static>> {
    if spec == "/" {
        return vec![key(spec)];
    }
    let mut spans = Vec::new();
    for (i, tok) in spec.split(' ').filter(|t| !t.is_empty()).enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(if tok == "/" {
            Span::styled("/", Style::new().fg(MUTED))
        } else {
            key(tok)
        });
    }
    spans
}

pub fn hint(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, (k, what)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(keys(k));
        spans.push(Span::styled(format!(" {what}"), Style::new().fg(OVERLAY)));
    }
    spans
}

pub fn dot() -> Span<'static> {
    Span::styled(" · ", Style::new().fg(MUTED))
}

pub fn panel<'a>(accent: Color, focused: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { accent } else { LINE }))
}

pub fn heading(text: &str, accent: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled("▍", Style::new().fg(accent)),
        Span::styled(text.to_string(), Style::new().fg(accent).bold()),
    ])
}

pub fn rule(text: &str, accent: Color, width: usize) -> Line<'static> {
    let used = text.chars().count() + 2;
    Line::from(vec![
        Span::styled(text.to_string(), Style::new().fg(accent).bold()),
        Span::raw(" "),
        Span::styled(
            "─".repeat(width.saturating_sub(used)),
            Style::new().fg(LINE),
        ),
    ])
}

pub fn centered(outer: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::vertical([Constraint::Length(height.min(outer.height))])
        .flex(Flex::Center)
        .areas(outer);
    let [area] = Layout::horizontal([Constraint::Length(width.min(outer.width))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn fade(c: Color, toward: (u8, u8, u8), keep: f32) -> Color {
    let (r, g, b) = match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => return MUTED,
    };
    let mix = |a: u8, z: u8| (a as f32 * keep + z as f32 * (1.0 - keep)).round() as u8;
    Color::Rgb(mix(r, toward.0), mix(g, toward.1), mix(b, toward.2))
}

pub fn dim_backdrop(f: &mut Frame) {
    let full = f.area();
    let area = Rect {
        height: full.height.saturating_sub(1),
        ..full
    };
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            let fg = if cell.fg == Color::Reset {
                MUTED
            } else {
                fade(cell.fg, BACKDROP, 0.35)
            };
            cell.fg = fg;
            if cell.bg != Color::Reset {
                cell.bg = fade(cell.bg, BACKDROP, 0.35);
            }
            cell.modifier.remove(Modifier::BOLD | Modifier::REVERSED);
        }
    }
}

fn shadow(f: &mut Frame, area: Rect) {
    let screen = f.area();
    let buf = f.buffer_mut();
    let right = Rect::new(area.right(), area.y + 1, 2, area.height);
    let bottom = Rect::new(area.x + 2, area.bottom(), area.width, 1);
    for r in [right, bottom] {
        let r = r.intersection(screen);
        for y in r.top()..r.bottom() {
            for x in r.left()..r.right() {
                let cell = &mut buf[(x, y)];
                cell.bg = CRUST;
                cell.fg = LINE;
            }
        }
    }
}

pub fn popup(f: &mut Frame, area: Rect, accent: Color, title: Line<'static>) -> Rect {
    dim_backdrop(f);
    shadow(f, area);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(accent).bg(PANEL))
        .style(Style::new().fg(TEXT).bg(PANEL))
        .padding(Padding::horizontal(2))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

pub fn popup_title(icon: &str, text: &str, accent: Color) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            format!(" {icon} {text} "),
            Style::new()
                .fg(CRUST)
                .bg(accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ])
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let len = line.chars().count();
        let w = word.chars().count();
        if len > 0 && len + 1 + w > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        while line.chars().count() > width {
            let head: String = line.chars().take(width).collect();
            let tail: String = line.chars().skip(width).collect();
            out.push(head);
            line = tail;
        }
    }
    out.push(line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_words_and_splits_long_ones() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("", 5), [""]);
    }

    #[test]
    fn app_badge_colours_by_liveness_and_owner() {
        let b = |l, o| app_badge(l, o);
        assert_eq!(b(Live::Up, None), ("▲ app".into(), GREEN));
        assert_eq!(b(Live::Partial, None), ("◐ app".into(), YELLOW));
        assert_eq!(b(Live::Down, None), ("▼ app".into(), MUTED));
        assert_eq!(b(Live::Up, Some("b-2")), ("▲ app (b-2)".into(), PEACH));
        assert_eq!(b(Live::Partial, Some("b-2")), ("◐ app (b-2)".into(), PEACH));
        assert_eq!(b(Live::Down, Some("b-2")), ("▼ app".into(), MUTED));
    }

    #[test]
    fn fades_toward_backdrop() {
        assert_eq!(
            fade(Color::Rgb(30, 30, 46), BACKDROP, 0.5),
            Color::Rgb(30, 30, 46)
        );
        assert_eq!(fade(Color::Red, BACKDROP, 0.5), MUTED);
    }
}
