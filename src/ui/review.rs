use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Meta,
    Hunk,
    Add,
    Del,
    Ctx,
}

pub struct Diff {
    pub id: String,
    pub lines: Vec<(Kind, String)>,
    pub files: Vec<(usize, String)>,
    pub scroll: usize,
}

impl Diff {
    pub fn parse(id: &str, text: &str) -> Self {
        let mut lines = Vec::new();
        let mut files = Vec::new();
        let mut in_header = false;
        for l in text.lines() {
            let kind = if let Some(rest) = l.strip_prefix("diff --git ") {
                in_header = true;
                let name = rest.rsplit(" b/").next().unwrap_or(rest).to_string();
                files.push((lines.len(), name));
                Kind::File
            } else if in_header && !l.starts_with("@@") {
                Kind::Meta
            } else if l.starts_with("@@") {
                in_header = false;
                Kind::Hunk
            } else if l.starts_with('+') {
                Kind::Add
            } else if l.starts_with('-') {
                Kind::Del
            } else {
                Kind::Ctx
            };
            lines.push((kind, l.to_string()));
        }
        Self {
            id: id.to_string(),
            lines,
            files,
            scroll: 0,
        }
    }

    pub fn current_file(&self) -> Option<usize> {
        self.files
            .iter()
            .rposition(|(start, _)| *start <= self.scroll)
    }

    pub fn next_file(&mut self) {
        if let Some((start, _)) = self.files.iter().find(|(s, _)| *s > self.scroll) {
            self.scroll = *start;
        }
    }

    pub fn prev_file(&mut self) {
        if let Some((start, _)) = self.files.iter().rev().find(|(s, _)| *s < self.scroll) {
            self.scroll = *start;
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    pub fn adds_dels(&self) -> (usize, usize) {
        self.lines.iter().fold((0, 0), |(a, d), (k, _)| match k {
            Kind::Add => (a + 1, d),
            Kind::Del => (a, d + 1),
            _ => (a, d),
        })
    }
}

pub fn render(f: &mut Frame, area: Rect, diff: &Diff, title: &str) {
    let (adds, dels) = diff.adds_dels();
    let file = diff
        .current_file()
        .map(|i| format!(" [{}/{}] {}", i + 1, diff.files.len(), diff.files[i].1))
        .unwrap_or_default();
    let block = Block::bordered().title(Line::from(vec![
        Span::styled(format!(" review {} ", diff.id), Style::new().bold()),
        Span::raw(format!("{title} ")),
        Span::styled(format!("+{adds} "), Style::new().fg(Color::Green)),
        Span::styled(format!("-{dels} "), Style::new().fg(Color::Red)),
        Span::styled(file, Style::new().fg(Color::Yellow)),
    ]));
    if diff.lines.is_empty() {
        f.render_widget(Paragraph::new("No changes.").block(block), area);
        return;
    }
    let height = area.height.saturating_sub(2) as usize;
    let text: Vec<Line> = diff
        .lines
        .iter()
        .skip(diff.scroll)
        .take(height)
        .map(|(k, l)| {
            let style = match k {
                Kind::File => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                Kind::Meta => Style::new().fg(Color::DarkGray),
                Kind::Hunk => Style::new().fg(Color::Cyan),
                Kind::Add => Style::new().fg(Color::Green),
                Kind::Del => Style::new().fg(Color::Red),
                Kind::Ctx => Style::new(),
            };
            Line::styled(l.replace('\t', "    "), style)
        })
        .collect();
    f.render_widget(Paragraph::new(text).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n ctx\ndiff --git a/b.txt b/b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+b\n";

    #[test]
    fn parses_files_and_counts_without_header_noise() {
        let d = Diff::parse("x", SAMPLE);
        assert_eq!(
            d.files.iter().map(|f| f.1.as_str()).collect::<Vec<_>>(),
            ["src/a.rs", "b.txt"]
        );
        assert_eq!(d.adds_dels(), (2, 1));
        assert_eq!(d.lines[2].0, Kind::Meta);
        assert_eq!(d.lines[11].0, Kind::Meta);
    }

    #[test]
    fn file_navigation() {
        let mut d = Diff::parse("x", SAMPLE);
        d.next_file();
        assert_eq!(d.current_file(), Some(1));
        d.next_file();
        assert_eq!(d.current_file(), Some(1));
        d.prev_file();
        assert_eq!(d.scroll, 0);
    }
}
