use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::checks::{Check, Status};
use crate::conflicts::Conflict;

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
    pub old_name: String,
    pub lines: Vec<DLine>,
    pub cursor: usize,
    pub rejected: BTreeSet<usize>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rejection {
    pub patch: String,
    pub restore: Vec<String>,
}

impl Rejection {
    pub fn is_empty(&self) -> bool {
        self.patch.is_empty() && self.restore.is_empty()
    }
}

impl FileDiff {
    pub fn units(&self) -> Vec<usize> {
        if self.name == COMMIT_MSG {
            return Vec::new();
        }
        let hunks: Vec<usize> = (0..self.lines.len())
            .filter(|&i| self.lines[i].kind == Kind::Hunk)
            .collect();
        if hunks.is_empty() { vec![0] } else { hunks }
    }

    pub fn unit_at(&self, i: usize) -> Option<usize> {
        let units = self.units();
        match units.as_slice() {
            [0] => Some(0),
            _ => units.into_iter().rev().find(|&u| u <= i),
        }
    }

    fn unit_end(&self, start: usize) -> usize {
        self.units()
            .into_iter()
            .find(|&u| u > start)
            .unwrap_or(self.lines.len())
    }

    fn unit_text(&self, start: usize) -> String {
        self.lines[start..self.unit_end(start)]
            .iter()
            .skip(usize::from(self.lines[start].kind == Kind::Hunk))
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn line_rejected(&self, i: usize) -> bool {
        self.unit_at(i).is_some_and(|u| self.rejected.contains(&u))
    }

    pub fn fully_rejected(&self) -> bool {
        let units = self.units();
        !units.is_empty() && units.iter().all(|u| self.rejected.contains(u))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub file: String,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub end_old: Option<usize>,
    pub end_new: Option<usize>,
    pub snippet: String,
    pub text: String,
    pub ai: bool,
}

impl Comment {
    pub fn multi_line(&self) -> bool {
        (self.old, self.new) != (self.end_old, self.end_new)
    }
}

pub const COMMIT_MSG: &str = "COMMIT_MSG";

const FEEDBACK_INSTRUCTIONS: &str = "Code review comments on your diff. Each comment is attached to a line: side \"new\" is a line in the new file, side \"old\" is a removed line from the old file. A comment with end_line covers every diff line from line through end_line (end_side tells which file end_line is in) and its code holds all of them. Comments on file \"COMMIT_MSG\" refer to lines of your commit messages. Address every comment and commit, then set the bead back to review.";

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    New,
    Old,
    #[default]
    File,
}

#[derive(Serialize)]
struct FeedbackComment<'a> {
    file: &'a str,
    line: Option<usize>,
    side: Side,
    old_line: Option<usize>,
    new_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_side: Option<Side>,
    code: &'a str,
    comment: &'a str,
}

fn side_line(old: Option<usize>, new: Option<usize>) -> (Side, Option<usize>) {
    match (new, old) {
        (Some(n), _) => (Side::New, Some(n)),
        (None, Some(o)) => (Side::Old, Some(o)),
        _ => (Side::File, None),
    }
}

impl<'a> From<&'a Comment> for FeedbackComment<'a> {
    fn from(c: &'a Comment) -> Self {
        let (side, line) = side_line(c.old, c.new);
        let (end_side, end_line) = match side_line(c.end_old, c.end_new) {
            (s, Some(l)) if c.multi_line() => (Some(s), Some(l)),
            _ => (None, None),
        };
        Self {
            file: &c.file,
            line,
            side,
            old_line: c.old,
            new_line: c.new,
            end_line,
            end_side,
            code: &c.snippet,
            comment: &c.text,
        }
    }
}

#[derive(Serialize)]
struct Feedback<'a> {
    bead: &'a str,
    instructions: &'a str,
    comments: Vec<FeedbackComment<'a>>,
}

pub fn feedback_json(bead: &str, comments: &[Comment]) -> Option<String> {
    if comments.is_empty() {
        return None;
    }
    let feedback = Feedback {
        bead,
        instructions: FEEDBACK_INSTRUCTIONS,
        comments: comments.iter().map(FeedbackComment::from).collect(),
    };
    serde_json::to_string(&feedback).ok()
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Approve,
    Changes,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AiComment {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub side: Side,
    #[serde(default)]
    pub line: Option<usize>,
    #[serde(default)]
    pub comment: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AiOutput {
    pub verdict: Verdict,
    #[serde(default)]
    pub comments: Vec<AiComment>,
}

pub fn parse_ai_review(text: &str) -> Option<AiOutput> {
    let text = text.trim();
    if let Ok(o) = serde_json::from_str(text) {
        return Some(o);
    }
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(text.get(start..=end)?).ok()
}

pub struct AiReview {
    pub verdict: Verdict,
    pub comments: Vec<Comment>,
    pub forwarded: bool,
}

pub fn verdict_badge(r: &AiReview) -> Span<'static> {
    match r.verdict {
        Verdict::Approve => Span::styled("✓ai ", Style::new().fg(Color::Green)),
        Verdict::Changes => Span::styled("✎ai ", Style::new().fg(Color::Yellow)),
    }
}

pub struct Diff {
    pub id: String,
    pub files: Vec<FileDiff>,
    pub sel: usize,
    pub comments: Vec<Comment>,
}

fn commentable(l: &DLine) -> bool {
    l.old.is_some() || l.new.is_some()
}

fn line_comment(file: &FileDiff, (start, end): (usize, usize), text: &str, ai: bool) -> Comment {
    let (s, e) = (&file.lines[start], &file.lines[end]);
    let code = |l: &DLine| l.text.get(1..).unwrap_or("").to_string();
    let snippet = if start == end {
        code(s).trim().to_string()
    } else {
        file.lines[start..=end]
            .iter()
            .filter(|l| commentable(l))
            .map(|l| code(l).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    Comment {
        file: file.name.clone(),
        old: s.old,
        new: s.new,
        end_old: e.old,
        end_new: e.new,
        snippet,
        text: text.to_string(),
        ai,
    }
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
                let name = rest.rsplit(" b/").next().unwrap_or(rest).to_string();
                let old_name = rest
                    .rsplit_once(" b/")
                    .and_then(|(a, _)| a.strip_prefix("a/"))
                    .unwrap_or(&name)
                    .to_string();
                files.push(FileDiff {
                    name,
                    old_name,
                    lines: Vec::new(),
                    cursor: 0,
                    rejected: BTreeSet::new(),
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

    pub fn with_commits(mut self, log: &str) -> Self {
        let log = log.trim_end();
        if log.is_empty() {
            return self;
        }
        let mut lines = Vec::new();
        let mut in_header = false;
        let mut n = 0;
        for l in log.lines() {
            let (kind, new, text) = if l.starts_with("commit ") {
                in_header = true;
                (Kind::File, None, l.to_string())
            } else if in_header {
                in_header = !l.is_empty();
                (Kind::Meta, None, l.to_string())
            } else {
                n += 1;
                (Kind::Ctx, Some(n), format!(" {l}"))
            };
            lines.push(DLine {
                kind,
                text,
                old: None,
                new,
            });
        }
        let cursor = lines.iter().position(|l| l.kind == Kind::Ctx).unwrap_or(0);
        self.files.insert(
            0,
            FileDiff {
                name: COMMIT_MSG.to_string(),
                old_name: COMMIT_MSG.to_string(),
                lines,
                cursor,
                rejected: BTreeSet::new(),
            },
        );
        self
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

    pub fn comment_span(&self, c: &Comment) -> Option<(usize, usize, usize)> {
        let Some(fi) = self.files.iter().position(|f| f.name == c.file) else {
            return (!self.files.is_empty()).then_some((0, 0, 0));
        };
        if c.old.is_none() && c.new.is_none() {
            return Some((fi, 0, 0));
        }
        let lines = &self.files[fi].lines;
        let find = |old, new| lines.iter().position(|l| l.old == old && l.new == new);
        let end = find(c.end_old, c.end_new).unwrap_or(0);
        let start = find(c.old, c.new).filter(|&s| s <= end).unwrap_or(end);
        Some((fi, start, end))
    }

    pub fn comment_pos(&self, c: &Comment) -> Option<(usize, usize)> {
        self.comment_span(c).map(|(fi, _, end)| (fi, end))
    }

    fn cursor_pos(&self) -> Option<(usize, usize)> {
        let f = self.files.get(self.sel)?;
        f.lines.get(f.cursor)?;
        Some((self.sel, f.cursor))
    }

    pub fn comment_range(&self, count: usize) -> Option<(usize, usize)> {
        let f = self.files.get(self.sel)?;
        let end = f.cursor.saturating_add(count.max(1)).min(f.lines.len());
        let mut hits = (f.cursor..end).filter(|&i| commentable(&f.lines[i]));
        let start = hits.next()?;
        Some((start, hits.next_back().unwrap_or(start)))
    }

    pub fn add_comment(&mut self, text: &str, count: usize) -> bool {
        let Some(range) = self.comment_range(count) else {
            return false;
        };
        let c = line_comment(&self.files[self.sel], range, text, false);
        self.comments.push(c);
        true
    }

    pub fn anchor(&self, c: &AiComment) -> Comment {
        let name = c.file.trim().trim_start_matches("./");
        let Some(f) = self.files.iter().find(|f| f.name == name) else {
            let text = if name.is_empty() || name == COMMIT_MSG {
                c.comment.clone()
            } else {
                match c.line {
                    Some(n) => format!("{name}:{n}: {}", c.comment),
                    None => format!("{name}: {}", c.comment),
                }
            };
            return Comment {
                file: COMMIT_MSG.to_string(),
                old: None,
                new: None,
                end_old: None,
                end_new: None,
                snippet: String::new(),
                text,
                ai: true,
            };
        };
        let hit = c.line.and_then(|n| {
            f.lines.iter().position(|l| match c.side {
                Side::New => l.new == Some(n),
                Side::Old => l.old == Some(n),
                Side::File => false,
            })
        });
        match hit {
            Some(i) => line_comment(f, (i, i), &c.comment, true),
            None => Comment {
                file: f.name.clone(),
                old: None,
                new: None,
                end_old: None,
                end_new: None,
                snippet: String::new(),
                text: match (c.side, c.line) {
                    (Side::File, _) | (_, None) => c.comment.clone(),
                    (_, Some(n)) => format!("line {n}: {}", c.comment),
                },
                ai: true,
            },
        }
    }

    pub fn comment_at_cursor(&self) -> Option<usize> {
        let pos = self.cursor_pos()?;
        self.comments
            .iter()
            .position(|c| self.comment_pos(c) == Some(pos))
    }

    pub fn delete_comments_at_cursor(&mut self) -> usize {
        let Some(pos) = self.cursor_pos() else {
            return 0;
        };
        let keep: Vec<bool> = self
            .comments
            .iter()
            .map(|c| self.comment_pos(c) != Some(pos))
            .collect();
        let before = self.comments.len();
        let mut keep = keep.into_iter();
        self.comments.retain(|_| keep.next().unwrap_or(true));
        before - self.comments.len()
    }

    pub fn feedback(&self) -> Option<String> {
        feedback_json(&self.id, &self.comments)
    }

    pub fn mark_hunk(&mut self, accept: bool) -> bool {
        let Some(f) = self.files.get_mut(self.sel) else {
            return false;
        };
        let Some(u) = f.unit_at(f.cursor) else {
            return self.mark_file(accept);
        };
        if accept {
            f.rejected.remove(&u);
        } else {
            f.rejected.insert(u);
        }
        match f.units().into_iter().find(|&n| n > u) {
            Some(n) => f.cursor = n,
            None => self.next_file(),
        }
        true
    }

    pub fn mark_file(&mut self, accept: bool) -> bool {
        let Some(f) = self.files.get_mut(self.sel) else {
            return false;
        };
        let units = f.units();
        if units.is_empty() {
            return false;
        }
        f.rejected = if accept {
            BTreeSet::new()
        } else {
            units.into_iter().collect()
        };
        self.next_file();
        true
    }

    pub fn rejected_hunks(&self) -> usize {
        self.files.iter().map(|f| f.rejected.len()).sum()
    }

    pub fn all_rejected(&self) -> bool {
        let mut files = self
            .files
            .iter()
            .filter(|f| !f.units().is_empty())
            .peekable();
        files.peek().is_some() && files.all(FileDiff::fully_rejected)
    }

    pub fn rejection(&self) -> Rejection {
        let mut r = Rejection::default();
        for f in self.files.iter().filter(|f| !f.rejected.is_empty()) {
            if f.fully_rejected() {
                if f.lines.iter().any(|l| l.text.starts_with("Binary files ")) {
                    r.restore.push(f.old_name.clone());
                    if f.name != f.old_name {
                        r.restore.push(f.name.clone());
                    }
                    continue;
                }
                for l in &f.lines {
                    r.patch.push_str(&l.text);
                    r.patch.push('\n');
                }
                continue;
            }
            let n = &f.name;
            r.patch
                .push_str(&format!("diff --git a/{n} b/{n}\n--- a/{n}\n+++ b/{n}\n"));
            for &u in &f.rejected {
                for l in &f.lines[u..f.unit_end(u)] {
                    r.patch.push_str(&l.text);
                    r.patch.push('\n');
                }
            }
        }
        r
    }

    pub fn carry_rejections(&mut self, old: &Diff) {
        for f in &mut self.files {
            let Some(o) = old.files.iter().find(|o| o.name == f.name) else {
                continue;
            };
            let gone: BTreeSet<String> = o.rejected.iter().map(|&u| o.unit_text(u)).collect();
            f.rejected = f
                .units()
                .into_iter()
                .filter(|&u| gone.contains(&f.unit_text(u)))
                .collect();
        }
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

fn wrap_into(
    rows: &mut Vec<Line<'static>>,
    prefix: Span<'static>,
    prefix_w: usize,
    text: &str,
    style: Style,
    width: usize,
) {
    let avail = width.saturating_sub(prefix_w).max(1);
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = chars.chunks(avail);
    let first: String = chunks.next().unwrap_or(&[]).iter().collect();
    rows.push(Line::from(vec![prefix, Span::styled(first, style)]));
    for c in chunks {
        rows.push(Line::from(vec![
            Span::raw(" ".repeat(prefix_w)),
            Span::styled(c.iter().collect::<String>(), style),
        ]));
    }
}

pub struct Header<'a> {
    pub title: &'a str,
    pub loc: Option<&'a str>,
    pub ai: Option<&'a AiReview>,
    pub conflict: Option<(&'a Conflict, &'a str)>,
    pub check: Option<&'a Check>,
    pub marked: Option<(usize, usize)>,
}

pub fn render(f: &mut Frame, area: Rect, diff: &Diff, h: Header) {
    let Header {
        title,
        loc,
        ai,
        conflict,
        check,
        marked,
    } = h;
    let (adds, dels) = diff.adds_dels();
    let mut spans = vec![
        Span::styled(format!(" review {} ", diff.id), Style::new().bold()),
        Span::raw(format!("{title} ")),
    ];
    if let Some(loc) = loc {
        spans.push(Span::styled(
            format!("[{loc}] "),
            Style::new().fg(Color::Blue),
        ));
    }
    spans.extend([
        Span::styled(format!("+{adds} "), Style::new().fg(Color::Green)),
        Span::styled(format!("-{dels} "), Style::new().fg(Color::Red)),
        Span::styled(
            format!("{} comments ", diff.comments.len()),
            Style::new().fg(Color::Magenta),
        ),
    ]);
    let rejected = diff.rejected_hunks();
    if rejected > 0 {
        spans.push(Span::styled(
            format!("✗{rejected} rejected "),
            Style::new().fg(Color::Red),
        ));
    }
    if let Some(r) = ai {
        spans.push(verdict_badge(r));
        if r.forwarded {
            spans.push(Span::styled(
                format!("{} sent ", r.comments.len()),
                Style::new().fg(Color::DarkGray),
            ));
        }
    }
    if let Some(c) = check {
        let color = match c.status {
            Status::Queued => Color::DarkGray,
            Status::Running => Color::Yellow,
            Status::Pass => Color::Green,
            Status::Fail => Color::Red,
        };
        spans.push(Span::styled(
            format!("{} ", c.summary()),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some((c, base)) = conflict {
        spans.push(Span::styled(
            format!("⚠ {} ", c.summary(base)),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    let conflicted = conflict.map(|(c, _)| c.files()).unwrap_or_default();
    let ai_label = if ai.is_some_and(|r| r.forwarded) {
        "ai (sent)"
    } else {
        "ai"
    };
    let block = Block::bordered().title(Line::from(spans));
    if diff.files.is_empty() {
        f.render_widget(Paragraph::new("No changes.").block(block), area);
        return;
    }
    let widest = diff.files.iter().map(|x| x.name.len()).max().unwrap_or(0) + 6;
    let side = (widest as u16).clamp(16, area.width / 3);
    let [left, right] =
        Layout::horizontal([Constraint::Length(side), Constraint::Fill(1)]).areas(area);

    let ranges: Vec<Option<(usize, usize, usize)>> =
        diff.comments.iter().map(|c| diff.comment_span(c)).collect();
    let pos: Vec<Option<(usize, usize)>> = ranges
        .iter()
        .map(|r| r.map(|(fi, _, end)| (fi, end)))
        .collect();
    let items: Vec<ListItem> = diff
        .files
        .iter()
        .enumerate()
        .map(|(fi, x)| {
            let base = x.name.rsplit('/').next().unwrap_or(&x.name);
            let n = pos
                .iter()
                .filter(|p| p.is_some_and(|(f, _)| f == fi))
                .count();
            let mut spans = vec![if conflicted.contains(x.name.as_str()) {
                Span::styled(format!("⚠ {base}"), Style::new().fg(Color::Red))
            } else if x.fully_rejected() {
                Span::styled(
                    format!("✗ {base}"),
                    Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::CROSSED_OUT),
                )
            } else if !x.rejected.is_empty() {
                Span::styled(format!("± {base}"), Style::new().fg(Color::Yellow))
            } else {
                Span::raw(base.to_string())
            }];
            if n > 0 {
                spans.push(Span::styled(
                    format!(" 💬{n}"),
                    Style::new().fg(Color::Magenta),
                ));
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
    let inner_w = right.width.saturating_sub(2) as usize;
    for (i, l) in file.lines.iter().enumerate() {
        if i == file.cursor {
            cursor_row = rows.len();
        }
        let num = |n: Option<usize>| n.map_or("    ".to_string(), |n| format!("{n:>4}"));
        let rejected = file.line_rejected(i);
        let mut style = if rejected {
            Style::new().fg(Color::DarkGray)
        } else {
            line_style(l.kind)
        };
        if rejected && matches!(l.kind, Kind::Add | Kind::Del) {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if i == file.cursor || marked.is_some_and(|(s, e)| (s..=e).contains(&i)) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let covered = ranges
            .iter()
            .any(|r| r.is_some_and(|(fi, s, e)| fi == diff.sel && s < e && (s..=e).contains(&i)));
        let gutter = if matches!(l.kind, Kind::Add | Kind::Del | Kind::Ctx) {
            format!("{} {} ", num(l.old), num(l.new))
        } else if rejected && file.unit_at(i) == Some(i) {
            " ✗ reject ".to_string()
        } else {
            "          ".to_string()
        };
        wrap_into(
            &mut rows,
            Span::styled(
                gutter,
                Style::new().fg(if rejected {
                    Color::Red
                } else if covered {
                    Color::Magenta
                } else {
                    Color::DarkGray
                }),
            ),
            10,
            &l.text.replace('\t', "    "),
            style,
            inner_w,
        );
        for (c, _) in diff
            .comments
            .iter()
            .zip(&pos)
            .filter(|(_, p)| **p == Some((diff.sel, i)))
        {
            let (text, color) = if c.ai {
                (format!("▍ {ai_label}: {}", c.text), Color::Cyan)
            } else {
                (format!("▍ {}", c.text), Color::Magenta)
            };
            wrap_into(
                &mut rows,
                Span::raw("           "),
                11,
                &text,
                Style::new().fg(color).add_modifier(Modifier::BOLD),
                inner_w,
            );
        }
    }
    let height = right.height.saturating_sub(2) as usize;
    let top = cursor_row
        .saturating_sub(height / 2)
        .min(rows.len().saturating_sub(height));
    let text: Vec<Line> = rows.into_iter().skip(top).take(height).collect();
    f.render_widget(
        Paragraph::new(text).block(Block::bordered().title(Span::styled(
            format!(" [{}/{}] {} ", diff.sel + 1, diff.files.len(), file.name),
            Style::new().fg(Color::Yellow),
        ))),
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

    const TWO_HUNKS: &str = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n@@ -9,2 +9,2 @@\n nine\n-ten\n+TEN\ndiff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\n";

    #[test]
    fn rejects_hunks_and_files() {
        let mut d = Diff::parse("x", TWO_HUNKS).with_commits("commit a\n\nmsg\n");
        assert!(!d.mark_hunk(false));
        d.next_file();
        assert_eq!(d.files[1].cursor, 4);
        assert!(d.mark_hunk(false));
        assert_eq!(d.files[1].cursor, 8);
        assert!(d.files[1].line_rejected(6) && !d.files[1].line_rejected(9));
        assert!(!d.files[1].line_rejected(1));
        assert!(d.mark_hunk(true));
        assert_eq!(d.sel, 2);
        assert!(d.mark_hunk(false));
        assert!(d.files[2].fully_rejected());
        assert_eq!(d.rejected_hunks(), 2);
        assert!(!d.all_rejected());

        let r = d.rejection();
        assert_eq!(
            r.patch,
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n-one\n+ONE\n two\ndiff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\n"
        );
        assert_eq!(r.restore, Vec::<String>::new());

        d.sel = 1;
        d.set_cursor(0);
        assert!(d.mark_hunk(false));
        assert!(d.files[1].fully_rejected());
        assert!(d.all_rejected());
        let r = d.rejection();
        assert!(
            r.patch
                .starts_with("diff --git a/a.rs b/a.rs\nindex 1..2 100644\n")
        );
        assert!(r.patch.ends_with("+TEN\ndiff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\n"));
        d.sel = 1;
        assert!(d.mark_file(true));
        assert_eq!(d.rejected_hunks(), 1);
        let bin = "diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n";
        let mut d = Diff::parse("x", bin);
        d.mark_file(false);
        assert_eq!(d.rejection().restore, ["i.png"]);
    }

    #[test]
    fn rejections_survive_reload_by_content() {
        let mut d = Diff::parse("x", TWO_HUNKS);
        d.set_cursor(8);
        d.mark_hunk(false);
        let shifted = TWO_HUNKS.replace("@@ -9,2 +9,2 @@", "@@ -12,2 +12,2 @@");
        let mut fresh = Diff::parse("x", &shifted);
        fresh.carry_rejections(&d);
        assert_eq!(fresh.files[0].rejected, BTreeSet::from([8]));
        let edited = TWO_HUNKS.replace("+TEN", "+Ten");
        let mut fresh = Diff::parse("x", &edited);
        fresh.carry_rejections(&d);
        assert!(fresh.files[0].rejected.is_empty());
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
        assert!(!d.add_comment("nope", 1));
        d.set_cursor(6);
        assert!(d.add_comment("rename this", 1));
        d.set_cursor(5);
        assert!(d.add_comment("why \"removed\"?", 1));
        let fb = d.feedback().unwrap();
        assert!(!fb.contains('\n'));
        let v: serde_json::Value = serde_json::from_str(&fb).unwrap();
        assert_eq!(v["bead"], "x");
        let c = &v["comments"];
        assert_eq!(c[0]["file"], "src/a.rs");
        assert_eq!(c[0]["line"], 1);
        assert_eq!(c[0]["side"], "new");
        assert_eq!(c[0]["code"], "new");
        assert_eq!(c[0]["comment"], "rename this");
        assert_eq!(c[1]["side"], "old");
        assert_eq!(c[1]["old_line"], 1);
        assert_eq!(c[1]["new_line"], serde_json::Value::Null);
        assert_eq!(c[1]["comment"], "why \"removed\"?");
        assert_eq!(d.delete_comments_at_cursor(), 1);
        d.set_cursor(6);
        assert_eq!(d.delete_comments_at_cursor(), 1);
        assert!(d.feedback().is_none());
    }

    #[test]
    fn counted_comments_span_lines() {
        let mut d = Diff::parse("x", TWO_HUNKS);
        d.set_cursor(4);
        assert_eq!(d.comment_range(1), None);
        assert_eq!(d.comment_range(2), Some((5, 5)));
        d.set_cursor(5);
        assert_eq!(d.comment_range(0), Some((5, 5)));
        assert_eq!(d.comment_range(10), Some((5, 11)));
        assert!(d.add_comment("spans hunks", 10));
        d.set_cursor(9);
        assert!(d.add_comment("one line", 1));
        let c = &d.comments[0];
        assert!(c.multi_line() && !d.comments[1].multi_line());
        assert_eq!(c.snippet, "one\nONE\ntwo\nnine\nten\nTEN");
        assert_eq!(d.comment_span(c), Some((0, 5, 11)));
        assert_eq!(d.comment_pos(c), Some((0, 11)));
        let v: serde_json::Value = serde_json::from_str(&d.feedback().unwrap()).unwrap();
        let c = &v["comments"];
        assert_eq!((&c[0]["side"], &c[0]["line"]), (&"old".into(), &1.into()));
        assert_eq!(
            (&c[0]["end_side"], &c[0]["end_line"]),
            (&"new".into(), &10.into())
        );
        assert!(c[1].get("end_line").is_none() && c[1].get("end_side").is_none());
        d.set_cursor(5);
        assert_eq!(d.comment_at_cursor(), None);
        d.set_cursor(11);
        assert_eq!(d.comment_at_cursor(), Some(0));
        assert_eq!(d.delete_comments_at_cursor(), 1);
    }

    #[test]
    fn commit_messages_become_first_file() {
        let log = "commit abc\nAuthor: a <a@a>\nDate:   now\n\nsubject\n\nbody\n\n";
        let mut d = Diff::parse("x", SAMPLE).with_commits(log);
        assert_eq!(d.files.len(), 3);
        assert_eq!(d.files[0].name, COMMIT_MSG);
        assert_eq!(d.adds_dels(), (2, 1));
        let l = &d.files[0].lines;
        assert_eq!(l[0].kind, Kind::File);
        assert_eq!(l[3].kind, Kind::Meta);
        assert_eq!((l[4].kind, l[4].new), (Kind::Ctx, Some(1)));
        assert_eq!(l.len(), 7);
        assert_eq!(d.files[0].cursor, 4);
        assert!(d.add_comment("reword", 1));
        let v: serde_json::Value = serde_json::from_str(&d.feedback().unwrap()).unwrap();
        assert_eq!(v["comments"][0]["file"], COMMIT_MSG);
        assert_eq!(v["comments"][0]["code"], "subject");
        d.next_file();
        assert_eq!(d.files[d.sel].name, "src/a.rs");
        assert_eq!(Diff::parse("x", SAMPLE).with_commits("\n").files.len(), 2);
    }

    #[test]
    fn parses_ai_review_output() {
        let approve = parse_ai_review(r#"  {"verdict":"approve","comments":[]}  "#).unwrap();
        assert_eq!(approve.verdict, Verdict::Approve);
        assert!(approve.comments.is_empty());

        let changes = parse_ai_review(
            r#"{"verdict":"changes","comments":[{"file":"src/a.rs","side":"new","line":1,"comment":"fix"},{"file":"b.txt","side":"file","line":null,"comment":"why"}]}"#,
        )
        .unwrap();
        assert_eq!(changes.verdict, Verdict::Changes);
        assert_eq!(
            changes.comments[0],
            AiComment {
                file: "src/a.rs".into(),
                side: Side::New,
                line: Some(1),
                comment: "fix".into(),
            }
        );
        assert_eq!(changes.comments[1].side, Side::File);
        assert_eq!(changes.comments[1].line, None);

        let fenced = parse_ai_review(
            "\n```json\n{\"verdict\":\"changes\",\"comments\":[{\"file\":\"x\",\"side\":\"old\",\"line\":3,\"comment\":\"c\"}]}\n```\n",
        )
        .unwrap();
        assert_eq!(fenced.comments[0].side, Side::Old);

        assert!(parse_ai_review("looks good to me").is_none());
        assert!(parse_ai_review("{not json}").is_none());
        assert!(parse_ai_review(r#"{"verdict":"maybe"}"#).is_none());
        assert!(parse_ai_review("").is_none());
    }

    fn ai(file: &str, side: Side, line: Option<usize>, comment: &str) -> AiComment {
        AiComment {
            file: file.into(),
            side,
            line,
            comment: comment.into(),
        }
    }

    #[test]
    fn anchors_ai_comments_like_human_ones() {
        let log = "commit abc\nAuthor: a <a@a>\nDate:   now\n\nsubject\n";
        let d = Diff::parse("x", SAMPLE).with_commits(log);

        let new = d.anchor(&ai("src/a.rs", Side::New, Some(1), "n"));
        assert_eq!(
            (new.old, new.new, new.snippet.as_str()),
            (None, Some(1), "new")
        );
        let ctx = d.anchor(&ai("src/a.rs", Side::New, Some(2), "c"));
        assert_eq!(
            (ctx.old, ctx.new, ctx.snippet.as_str()),
            (Some(2), Some(2), "ctx")
        );
        let old = d.anchor(&ai("./src/a.rs", Side::Old, Some(1), "o"));
        assert_eq!(
            (old.file.as_str(), old.old, old.new, old.snippet.as_str()),
            ("src/a.rs", Some(1), None, "old")
        );
        let msg = d.anchor(&ai(COMMIT_MSG, Side::New, Some(1), "m"));
        assert_eq!((msg.new, msg.snippet.as_str()), (Some(1), "subject"));
        assert!(new.ai && old.ai);

        let missing_line = d.anchor(&ai("src/a.rs", Side::New, Some(99), "far"));
        assert_eq!(
            missing_line,
            Comment {
                file: "src/a.rs".into(),
                old: None,
                new: None,
                end_old: None,
                end_new: None,
                snippet: String::new(),
                text: "line 99: far".into(),
                ai: true,
            }
        );
        let file_level = d.anchor(&ai("b.txt", Side::File, None, "whole file"));
        assert_eq!((file_level.file.as_str(), file_level.new), ("b.txt", None));
        assert_eq!(file_level.text, "whole file");
        let unknown = d.anchor(&ai("nope.rs", Side::New, Some(3), "lost"));
        assert_eq!(unknown.file, COMMIT_MSG);
        assert_eq!((unknown.old, unknown.new), (None, None));
        assert_eq!(unknown.text, "nope.rs:3: lost");

        let v: serde_json::Value =
            serde_json::from_str(&feedback_json("x", &[missing_line]).unwrap()).unwrap();
        assert_eq!(v["comments"][0]["side"], "file");
        assert_eq!(v["comments"][0]["line"], serde_json::Value::Null);
    }

    #[test]
    fn ai_and_human_feedback_are_identical() {
        let mut d = Diff::parse("x", SAMPLE);
        d.set_cursor(6);
        assert!(d.add_comment("rename this", 1));
        d.set_cursor(5);
        assert!(d.add_comment("why removed", 1));
        let human = d.feedback().unwrap();
        let ai_comments = vec![
            d.anchor(&ai("src/a.rs", Side::New, Some(1), "rename this")),
            d.anchor(&ai("src/a.rs", Side::Old, Some(1), "why removed")),
        ];
        assert_eq!(feedback_json("x", &ai_comments).unwrap(), human);
        d.comments.iter_mut().for_each(|c| c.ai = true);
        assert_eq!(d.feedback().unwrap(), human);
    }

    #[test]
    fn file_level_comments_sit_on_file_header() {
        let mut d = Diff::parse("x", SAMPLE);
        d.comments
            .push(d.anchor(&ai("src/a.rs", Side::File, None, "f")));
        d.set_cursor(4);
        assert_eq!(d.comment_at_cursor(), None);
        d.set_cursor(0);
        assert_eq!(d.comment_at_cursor(), Some(0));
        assert_eq!(d.delete_comments_at_cursor(), 1);
    }

    #[test]
    fn stale_comments_fall_back_to_file_header() {
        let mut d = Diff::parse("x", SAMPLE);
        let stale = |file: &str| Comment {
            file: file.into(),
            old: None,
            new: Some(42),
            end_old: None,
            end_new: Some(42),
            snippet: String::new(),
            text: "stale".into(),
            ai: true,
        };
        d.comments.push(stale("b.txt"));
        d.comments.push(stale("gone.rs"));
        assert_eq!(d.comment_pos(&d.comments[0]), Some((1, 0)));
        assert_eq!(d.comment_pos(&d.comments[1]), Some((0, 0)));
        d.set_cursor(0);
        assert_eq!(d.comment_at_cursor(), Some(1));
        d.next_file();
        d.set_cursor(0);
        assert_eq!(d.comment_at_cursor(), Some(0));
        assert_eq!(d.delete_comments_at_cursor(), 1);
        assert_eq!(d.comments.len(), 1);
    }
}
