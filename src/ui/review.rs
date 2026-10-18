use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Meta,
    Hunk,
    Add,
    Del,
    Ctx,
}

pub struct DLine {
    pub kind: Kind,
    pub text: String,
    pub old: Option<usize>,
    pub new: Option<usize>,
}

pub struct FileDiff {
    pub name: String,
    pub lines: Vec<DLine>,
    pub cursor: usize,
}

pub struct Comment {
    pub file: String,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub snippet: String,
    pub text: String,
}

impl Comment {
    fn location(&self) -> String {
        match (self.new, self.old) {
            (Some(n), _) => format!("{}:{n}", self.file),
            (None, Some(o)) => format!("{}:{o} (removed line)", self.file),
            _ => self.file.clone(),
        }
    }
}

pub struct Diff {
    pub id: String,
    pub files: Vec<FileDiff>,
    pub sel: usize,
    pub comments: Vec<Comment>,
}

fn hunk_start(l: &str) -> (usize, usize) {
    let mut nums = l
        .split_whitespace()
        .skip(1)
        .take(2)
        .map(|p| p[1..].split(',').next().and_then(|n| n.parse().ok()));
    (
        nums.next().flatten().unwrap_or(1),
        nums.next().flatten().unwrap_or(1),
    )
}

impl Diff {
    pub fn parse(id: &str, text: &str) -> Self {
        let mut files: Vec<FileDiff> = Vec::new();
        let mut in_header = false;
        let (mut old, mut new) = (0, 0);
        for l in text.lines() {
            if let Some(rest) = l.strip_prefix("diff --git ") {
                in_header = true;
                files.push(FileDiff {
                    name: rest.rsplit(" b/").next().unwrap_or(rest).to_string(),
                    lines: Vec::new(),
                    cursor: 0,
                });
            }
            let Some(file) = files.last_mut() else {
                continue;
            };
            let (kind, o, n) = if l.starts_with("diff --git ") {
                (Kind::File, None, None)
            } else if in_header && !l.starts_with("@@") {
                (Kind::Meta, None, None)
            } else if l.starts_with("@@") {
                in_header = false;
                (old, new) = hunk_start(l);
                (Kind::Hunk, None, None)
            } else if l.starts_with('+') {
                new += 1;
                (Kind::Add, None, Some(new - 1))
            } else if l.starts_with('-') {
                old += 1;
                (Kind::Del, Some(old - 1), None)
            } else {
                old += 1;
                new += 1;
                (Kind::Ctx, Some(old - 1), Some(new - 1))
            };
            file.lines.push(DLine {
                kind,
                text: l.to_string(),
                old: o,
                new: n,
            });
        }
        for f in &mut files {
            f.cursor = f
                .lines
                .iter()
                .position(|l| l.kind == Kind::Hunk)
                .unwrap_or(0);
        }
        Self {
            id: id.to_string(),
            files,
            sel: 0,
            comments: Vec::new(),
        }
    }

    pub fn next_file(&mut self) {
        if self.sel + 1 < self.files.len() {
            self.sel += 1;
        }
    }

    pub fn prev_file(&mut self) {
        self.sel = self.sel.saturating_sub(1);
    }

    pub fn move_cursor(&mut self, delta: isize) {
        if let Some(f) = self.files.get_mut(self.sel) {
            let max = f.lines.len().saturating_sub(1) as isize;
            f.cursor = (f.cursor as isize + delta).clamp(0, max) as usize;
        }
    }

    pub fn set_cursor(&mut self, pos: usize) {
        self.move_cursor(0);
        if let Some(f) = self.files.get_mut(self.sel) {
            f.cursor = pos.min(f.lines.len().saturating_sub(1));
        }
    }

    fn comment_matches(c: &Comment, file: &FileDiff, l: &DLine) -> bool {
        c.file == file.name && c.old == l.old && c.new == l.new
    }

    pub fn add_comment(&mut self, text: &str) -> bool {
        let Some(f) = self.files.get(self.sel) else {
            return false;
        };
        let Some(l) = f.lines.get(f.cursor) else {
            return false;
        };
        if l.old.is_none() && l.new.is_none() {
            return false;
        }
        let c = Comment {
            file: f.name.clone(),
            old: l.old,
            new: l.new,
            snippet: l.text.get(1..).unwrap_or("").trim().to_string(),
            text: text.to_string(),
        };
        self.comments.push(c);
        true
    }

    pub fn delete_comments_at_cursor(&mut self) -> usize {
        let Some(f) = self.files.get(self.sel) else {
            return 0;
        };
        let Some(l) = f.lines.get(f.cursor) else {
            return 0;
        };
        let before = self.comments.len();
        self.comments.retain(|c| !Self::comment_matches(c, f, l));
        before - self.comments.len()
    }

    pub fn comment_count(&self, file: &str) -> usize {
        self.comments.iter().filter(|c| c.file == file).count()
    }

    pub fn feedback(&self) -> Option<String> {
        if self.comments.is_empty() {
            return None;
        }
        let items: Vec<String> = self
            .comments
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "({}) {} `{}`: {}",
                    i + 1,
                    c.location(),
                    c.snippet.replace('`', "'"),
                    c.text
                )
            })
            .collect();
        Some(format!(
            "Code review comments on your diff, each as (n) file:line `code`: comment. Address every one and commit, then set the bead back to review. {}",
            items.join(" ; ")
        ))
    }

    pub fn adds_dels(&self) -> (usize, usize) {
        self.files
            .iter()
            .flat_map(|f| &f.lines)
            .fold((0, 0), |(a, d), l| match l.kind {
                Kind::Add => (a + 1, d),
                Kind::Del => (a, d + 1),
                _ => (a, d),
            })
    }
}

fn line_style(k: Kind) -> Style {
    match k {
        Kind::File => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Kind::Meta => Style::new().fg(Color::DarkGray),
        Kind::Hunk => Style::new().fg(Color::Cyan),
        Kind::Add => Style::new().fg(Color::Green),
        Kind::Del => Style::new().fg(Color::Red),
        Kind::Ctx => Style::new(),
    }
}

pub fn render(f: &mut Frame, area: Rect, diff: &Diff, title: &str) {
    let (adds, dels) = diff.adds_dels();
    let block = Block::bordered().title(Line::from(vec![
        Span::styled(format!(" review {} ", diff.id), Style::new().bold()),
        Span::raw(format!("{title} ")),
        Span::styled(format!("+{adds} "), Style::new().fg(Color::Green)),
        Span::styled(format!("-{dels} "), Style::new().fg(Color::Red)),
        Span::styled(
            format!("{} comments ", diff.comments.len()),
            Style::new().fg(Color::Magenta),
        ),
    ]));
    if diff.files.is_empty() {
        f.render_widget(Paragraph::new("No changes.").block(block), area);
        return;
    }
    let widest = diff.files.iter().map(|x| x.name.len()).max().unwrap_or(0) + 6;
    let side = (widest as u16).clamp(16, area.width / 3);
    let [left, right] =
        Layout::horizontal([Constraint::Length(side), Constraint::Fill(1)]).areas(area);

    let items: Vec<ListItem> = diff
        .files
        .iter()
        .map(|x| {
            let base = x.name.rsplit('/').next().unwrap_or(&x.name);
            let n = diff.comment_count(&x.name);
            let mut spans = vec![Span::raw(base.to_string())];
            if n > 0 {
                spans.push(Span::styled(format!(" 💬{n}"), Style::new().fg(Color::Magenta)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(diff.sel));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title(" files "))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        left,
        &mut state,
    );

    let file = &diff.files[diff.sel];
    let mut rows: Vec<Line> = Vec::new();
    let mut cursor_row = 0;
    for (i, l) in file.lines.iter().enumerate() {
        if i == file.cursor {
            cursor_row = rows.len();
        }
        let num = |n: Option<usize>| n.map_or("    ".to_string(), |n| format!("{n:>4}"));
        let mut style = line_style(l.kind);
        if i == file.cursor {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let gutter = if matches!(l.kind, Kind::Add | Kind::Del | Kind::Ctx) {
            format!("{} {} ", num(l.old), num(l.new))
        } else {
            "          ".to_string()
        };
        rows.push(Line::from(vec![
            Span::styled(gutter, Style::new().fg(Color::DarkGray)),
            Span::styled(l.text.replace('\t', "    "), style),
        ]));
        for c in diff
            .comments
            .iter()
            .filter(|c| Diff::comment_matches(c, file, l))
        {
            rows.push(Line::from(vec![
                Span::raw("           "),
                Span::styled(
                    format!("▍ {}", c.text),
                    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }
    let height = right.height.saturating_sub(2) as usize;
    let top = cursor_row
        .saturating_sub(height / 2)
        .min(rows.len().saturating_sub(height));
    let text: Vec<Line> = rows.into_iter().skip(top).take(height).collect();
    f.render_widget(
        Paragraph::new(text).block(
            Block::bordered().title(Span::styled(
                format!(" [{}/{}] {} ", diff.sel + 1, diff.files.len(), file.name),
                Style::new().fg(Color::Yellow),
            )),
        ),
        right,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n ctx\ndiff --git a/b.txt b/b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+b\n";

    #[test]
    fn parses_files_and_counts_without_header_noise() {
        let d = Diff::parse("x", SAMPLE);
        assert_eq!(
            d.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["src/a.rs", "b.txt"]
        );
        assert_eq!(d.adds_dels(), (2, 1));
        assert_eq!(d.files[0].lines[2].kind, Kind::Meta);
        assert_eq!(d.files[1].lines[3].kind, Kind::Meta);
        assert_eq!(d.files[0].cursor, 4);
    }

    #[test]
    fn tracks_line_numbers() {
        let d = Diff::parse("x", SAMPLE);
        let l = &d.files[0].lines;
        assert_eq!((l[5].old, l[5].new), (Some(1), None));
        assert_eq!((l[6].old, l[6].new), (None, Some(1)));
        assert_eq!((l[7].old, l[7].new), (Some(2), Some(2)));
    }

    #[test]
    fn file_navigation() {
        let mut d = Diff::parse("x", SAMPLE);
        d.next_file();
        assert_eq!(d.sel, 1);
        d.next_file();
        assert_eq!(d.sel, 1);
        d.prev_file();
        assert_eq!(d.sel, 0);
    }

    #[test]
    fn comments_render_into_feedback() {
        let mut d = Diff::parse("x", SAMPLE);
        d.set_cursor(0);
        assert!(!d.add_comment("nope"));
        d.set_cursor(6);
        assert!(d.add_comment("rename this"));
        let fb = d.feedback().unwrap();
        assert!(fb.contains("(1) src/a.rs:1 `new`: rename this"));
        assert_eq!(d.comment_count("src/a.rs"), 1);
        assert_eq!(d.delete_comments_at_cursor(), 1);
        assert!(d.feedback().is_none());
    }
}
