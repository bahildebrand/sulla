use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use color_eyre::Result;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::theme;
use crate::config::Config;
use crate::config::effective::{Layers, Source};
use crate::config::reload;
use crate::config::show::{display_path, dotted, key, source_color, value_color};

const ACCENTS: [Color; 8] = [
    theme::BLUE,
    theme::MAUVE,
    theme::TEAL,
    theme::PEACH,
    theme::PINK,
    theme::SKY,
    theme::LAVENDER,
    theme::GREEN,
];
const PILL_W: usize = 9;
const GENERAL: &str = "General";

#[derive(Debug, Clone)]
pub struct Item {
    pub path: Vec<String>,
    pub key: String,
    pub value: Option<toml::Value>,
    pub source: Source,
    pub file: Option<String>,
    pub restart: Option<Option<toml::Value>>,
}

impl Item {
    pub fn dotted(&self) -> String {
        dotted(&self.path)
    }

    fn lines(&self) -> Option<Vec<&str>> {
        let Some(toml::Value::String(s)) = &self.value else {
            return None;
        };
        let lines: Vec<&str> = s.trim_end_matches('\n').split('\n').collect();
        (lines.len() > 1).then_some(lines)
    }
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub accent: Color,
    pub items: Vec<Item>,
    pub unknown: bool,
}

#[derive(Debug, Clone)]
struct File {
    source: Source,
    path: String,
    exists: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Header(usize),
    Item(usize, usize),
}

#[derive(Debug)]
pub struct ConfigView {
    files: Vec<File>,
    flags: Vec<(String, toml::Value)>,
    pub sections: Vec<Section>,
    folded: HashSet<String>,
    expanded: HashSet<String>,
    pub hide_defaults: bool,
    pub filter: String,
    pub typing: bool,
    cursor: usize,
    pub flash: Option<String>,
    offset: Cell<usize>,
    reload: reload::State,
}

impl ConfigView {
    pub fn new(base: &Layers, cfg: &Config, state: &reload::State) -> Result<Self> {
        let layers = base.with_runtime(cfg)?;
        let eff = layers.effective()?;
        let mut tables: BTreeMap<Vec<String>, Vec<Item>> = BTreeMap::new();
        let mut add = |path: Vec<String>, value: Option<toml::Value>, source: Source| {
            let Some((leaf, table)) = path.split_last() else {
                return;
            };
            tables.entry(table.to_vec()).or_default().push(Item {
                key: key(leaf),
                path,
                value,
                source,
                file: None,
                restart: None,
            });
        };
        let mut unset = Vec::new();
        nulls(&serde_json::to_value(cfg)?, &mut Vec::new(), &mut unset);
        let parents: HashSet<&[String]> = unset
            .iter()
            .chain(state.restart.keys())
            .flat_map(|p| (1..p.len()).map(move |n| &p[..n]))
            .collect();
        for e in eff.entries {
            let empty = matches!(&e.value, toml::Value::Table(t) if t.is_empty());
            if !(empty && parents.contains(e.path.as_slice())) {
                add(e.path, Some(e.value), e.source);
            }
        }
        for path in unset {
            let source = match layers.source(&path) {
                Source::Default => Source::Default,
                _ => Source::Runtime,
            };
            add(path, None, source);
        }
        let mut pending = state.restart.clone();
        for item in tables.values_mut().flatten() {
            if let Some(new) = pending.remove(&item.path) {
                item.source = base.source(&item.path);
                item.restart = Some(new);
            }
        }
        for (path, new) in pending {
            let Some((leaf, table)) = path.split_last() else {
                continue;
            };
            tables.entry(table.to_vec()).or_default().push(Item {
                key: key(leaf),
                source: base.source(&path),
                path,
                value: None,
                file: None,
                restart: Some(new),
            });
        }
        let mut sections = Vec::new();
        let mut tops: Vec<String> = Vec::new();
        for (table, mut items) in tables {
            items.sort_by(|a, b| a.key.cmp(&b.key));
            let top = table.first().cloned().unwrap_or_default();
            if !tops.contains(&top) {
                tops.push(top.clone());
            }
            let accent = ACCENTS[tops.iter().position(|t| *t == top).unwrap_or(0) % ACCENTS.len()];
            let name = if table.is_empty() {
                GENERAL.to_string()
            } else {
                dotted(&table)
            };
            sections.push(Section {
                name,
                accent,
                items,
                unknown: false,
            });
        }
        if !eff.unknown.is_empty() {
            sections.push(Section {
                name: "Unknown keys".into(),
                accent: theme::RED,
                items: eff
                    .unknown
                    .into_iter()
                    .map(|u| Item {
                        key: dotted(&u.path),
                        path: u.path,
                        value: Some(u.value),
                        source: u.source,
                        file: Some(display_path(&u.file)),
                        restart: None,
                    })
                    .collect(),
                unknown: true,
            });
        }
        let mut files = Vec::new();
        if layers.global.is_none() {
            files.push(File {
                source: Source::Global,
                path: "no config directory".into(),
                exists: None,
            });
        }
        files.extend(layers.files().map(|(source, l)| File {
            source,
            path: display_path(&l.path),
            exists: Some(l.exists()),
        }));
        Ok(Self {
            files,
            flags: layers
                .flags
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            sections,
            folded: HashSet::new(),
            expanded: HashSet::new(),
            hide_defaults: false,
            filter: String::new(),
            typing: false,
            cursor: 0,
            flash: None,
            offset: Cell::new(0),
            reload: state.clone(),
        })
    }

    pub fn keep_ui(&mut self, old: &ConfigView) {
        self.folded = old.folded.clone();
        self.expanded = old.expanded.clone();
        self.hide_defaults = old.hide_defaults;
        self.filter = old.filter.clone();
        self.typing = old.typing;
        self.cursor = old.cursor;
        self.offset.set(old.offset.get());
        self.retarget(None);
    }

    pub fn flashing(&self, item: &Item) -> bool {
        self.reload.flashing(&item.path, Instant::now())
    }

    fn visible(&self, item: &Item) -> bool {
        if self.hide_defaults && item.source == Source::Default {
            return false;
        }
        self.filter.is_empty()
            || item
                .path
                .join(".")
                .to_lowercase()
                .contains(&self.filter.to_lowercase())
    }

    fn shown(&self, si: usize) -> usize {
        self.sections[si]
            .items
            .iter()
            .filter(|i| self.visible(i))
            .count()
    }

    fn is_open(&self, si: usize) -> bool {
        !self.filter.is_empty() || !self.folded.contains(&self.sections[si].name)
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for (si, s) in self.sections.iter().enumerate() {
            let items: Vec<usize> = (0..s.items.len())
                .filter(|&i| self.visible(&s.items[i]))
                .collect();
            if items.is_empty() {
                continue;
            }
            out.push(Row::Header(si));
            if self.is_open(si) {
                out.extend(items.into_iter().map(|i| Row::Item(si, i)));
            }
        }
        out
    }

    pub fn current(&self) -> Option<Row> {
        self.rows().get(self.cursor).copied()
    }

    pub fn item(&self, row: Row) -> Option<&Item> {
        match row {
            Row::Item(s, i) => self.sections.get(s)?.items.get(i),
            Row::Header(_) => None,
        }
    }

    fn retarget(&mut self, keep: Option<Row>) {
        let rows = self.rows();
        if let Some(r) = keep {
            let header = match r {
                Row::Item(s, _) | Row::Header(s) => Row::Header(s),
            };
            if let Some(i) = rows
                .iter()
                .position(|x| *x == r)
                .or_else(|| rows.iter().position(|x| *x == header))
            {
                self.cursor = i;
            }
        }
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
    }

    fn change(&mut self, f: impl FnOnce(&mut Self)) {
        let keep = self.current();
        f(self);
        self.retarget(keep);
    }

    pub fn move_by(&mut self, delta: isize) {
        let last = self.rows().len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn top(&mut self) {
        self.cursor = 0;
    }

    pub fn bottom(&mut self) {
        self.cursor = self.rows().len().saturating_sub(1);
    }

    #[cfg(test)]
    pub fn select(&mut self, row: Row) -> bool {
        match self.rows().iter().position(|r| *r == row) {
            Some(i) => {
                self.cursor = i;
                true
            }
            None => false,
        }
    }

    pub fn next_section(&mut self, forward: bool) {
        let rows = self.rows();
        let mut headers = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Row::Header(_)))
            .map(|(i, _)| i);
        let target = if forward {
            headers.find(|&i| i > self.cursor)
        } else {
            headers.rfind(|&i| i < self.cursor)
        };
        if let Some(i) = target {
            self.cursor = i;
        }
    }

    pub fn toggle(&mut self) -> bool {
        match self.current() {
            Some(Row::Header(s)) => {
                self.toggle_section(s);
                true
            }
            Some(r @ Row::Item(..)) => {
                let Some(item) = self.item(r).filter(|i| i.lines().is_some()) else {
                    return false;
                };
                let k = item.dotted();
                if !self.expanded.remove(&k) {
                    self.expanded.insert(k);
                }
                true
            }
            None => false,
        }
    }

    fn toggle_section(&mut self, s: usize) {
        let name = self.sections[s].name.clone();
        self.change(|v| {
            if !v.folded.remove(&name) {
                v.folded.insert(name);
            }
        });
        if let Some(i) = self.rows().iter().position(|r| *r == Row::Header(s)) {
            self.cursor = i;
        }
    }

    pub fn toggle_fold(&mut self) {
        if let Some(Row::Header(s) | Row::Item(s, _)) = self.current() {
            self.toggle_section(s);
        }
    }

    pub fn fold_all(&mut self, fold: bool) {
        self.change(|v| {
            v.folded = if fold {
                v.sections.iter().map(|s| s.name.clone()).collect()
            } else {
                HashSet::new()
            };
        });
    }

    pub fn toggle_defaults(&mut self) {
        self.change(|v| v.hide_defaults = !v.hide_defaults);
    }

    pub fn set_filter(&mut self, filter: String) {
        self.change(|v| v.filter = filter);
        if !self.filter.is_empty()
            && let Some(i) = self.rows().iter().position(|r| matches!(r, Row::Item(..)))
        {
            self.cursor = i;
        }
    }

    pub fn yank(&self) -> Option<String> {
        match self.current()? {
            Row::Header(s) => Some(self.sections[s].name.clone()),
            r => {
                let item = self.item(r)?;
                Some(match &item.value {
                    Some(v) => format!("{} = {}", item.dotted(), inline(v)),
                    None => format!("{} is unset", item.dotted()),
                })
            }
        }
    }

    fn matches(&self) -> usize {
        (0..self.sections.len()).map(|s| self.shown(s)).sum()
    }
}

fn nulls(v: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match v {
        serde_json::Value::Null => out.push(path.clone()),
        serde_json::Value::Object(m) => {
            for (k, v) in m {
                path.push(k.clone());
                nulls(v, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn inline(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => quote(s),
        toml::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(inline).collect();
            format!("[{}]", items.join(", "))
        }
        toml::Value::Table(t) if t.is_empty() => "{}".into(),
        toml::Value::Table(t) => {
            let items: Vec<String> = t
                .iter()
                .map(|(k, v)| format!("{} = {}", key(k), inline(v)))
                .collect();
            format!("{{ {} }}", items.join(", "))
        }
        v => v.to_string(),
    }
}

fn bare(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        toml::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(bare).collect();
            format!("[{}]", items.join(", "))
        }
        v => inline(v),
    }
}

fn text_width(s: &str) -> usize {
    Span::raw(s).width()
}

fn clip(s: &str, max: usize) -> String {
    if text_width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.chars() {
        if text_width(&out) + text_width(&c.to_string()) + 1 > max {
            break;
        }
        out.push(c);
    }
    if max > 0 {
        out.push('…');
    }
    out
}

fn marked(text: &str, query: &str, style: Style) -> Vec<Span<'static>> {
    let mark = Style::new().fg(theme::CRUST).bg(theme::YELLOW).bold();
    let lower = text.to_lowercase();
    let q = query.to_lowercase();
    if q.is_empty() || lower.len() != text.len() {
        return vec![Span::styled(text.to_string(), style)];
    }
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(i) = lower[at..].find(&q) {
        let start = at + i;
        let end = start + q.len();
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        if start > at {
            spans.push(Span::styled(text[at..start].to_string(), style));
        }
        spans.push(Span::styled(text[start..end].to_string(), mark));
        at = end;
    }
    if at < text.len() {
        spans.push(Span::styled(text[at..].to_string(), style));
    }
    spans
}

fn value_spans(item: &Item, budget: usize) -> Vec<Span<'static>> {
    let Some(v) = &item.value else {
        return vec![Span::styled(
            clip("none", budget),
            Style::new().fg(theme::MUTED).italic(),
        )];
    };
    if item.file.is_some() {
        let file = format!("  {}", item.file.as_deref().unwrap_or_default());
        let text = clip(&inline(v), budget);
        let rest = budget.saturating_sub(text_width(&text));
        return vec![
            Span::styled(text, Style::new().fg(theme::RED)),
            Span::styled(clip(&file, rest), Style::new().fg(theme::OVERLAY)),
        ];
    }
    if let Some(lines) = item.lines() {
        let more = format!(" … (+{} lines)", lines.len() - 1);
        let first = clip(lines[0], budget.saturating_sub(text_width(&more)));
        let rest = budget.saturating_sub(text_width(&first));
        return vec![
            Span::styled(first, Style::new().fg(theme::GREEN)),
            Span::styled(clip(&more, rest), Style::new().fg(theme::OVERLAY).italic()),
        ];
    }
    let text = match v {
        toml::Value::Boolean(true) => "✓ true".to_string(),
        toml::Value::Boolean(false) => "✗ false".to_string(),
        toml::Value::Array(_) => bare(v),
        v => inline(v),
    };
    vec![Span::styled(
        clip(&text, budget),
        Style::new().fg(value_color(v)),
    )]
}

struct Body {
    lines: Vec<Line<'static>>,
    cursor: Option<(usize, usize)>,
}

fn highlight(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    let mut spans: Vec<Span<'static>> = line.spans;
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    Line::from(
        spans
            .into_iter()
            .map(|mut s| {
                if s.style.bg.is_none() {
                    s.style = s.style.bg(theme::CURSOR_BG);
                }
                s
            })
            .collect::<Vec<_>>(),
    )
}

impl ConfigView {
    fn header_line(&self, s: usize, width: usize, cursor: bool) -> Line<'static> {
        let sec = &self.sections[s];
        let glyph = if self.is_open(s) { "▾" } else { "▸" };
        let style = Style::new().fg(sec.accent).add_modifier(Modifier::BOLD);
        let name = clip(&sec.name, width.saturating_sub(12).max(1));
        let mut spans = vec![Span::styled(format!("{glyph} "), style)];
        let query = if sec.unknown || sec.name == GENERAL {
            ""
        } else {
            &self.filter
        };
        spans.extend(marked(&name, query, style));
        spans.push(Span::styled(
            format!(" ({}) ", self.shown(s)),
            Style::new().fg(theme::OVERLAY),
        ));
        let used: usize = spans.iter().map(Span::width).sum();
        let rule_style = Style::new().fg(if cursor { sec.accent } else { theme::LINE });
        spans.push(Span::styled(
            "─".repeat(width.saturating_sub(used)),
            rule_style,
        ));
        Line::from(spans)
    }

    fn item_line(&self, item: &Item, key_w: usize, width: usize, cursor: bool) -> Line<'static> {
        let style = if item.file.is_some() {
            Style::new().fg(theme::RED)
        } else if self.flashing(item) {
            Style::new().fg(theme::YELLOW).bold()
        } else if cursor {
            Style::new().fg(theme::TEXT).bold()
        } else {
            Style::new().fg(theme::TEXT)
        };
        let k = clip(&item.key, key_w);
        let mut spans = vec![Span::raw("  ")];
        spans.extend(marked(&k, &self.filter, style));
        spans.push(Span::raw(
            " ".repeat(key_w.saturating_sub(text_width(&k)) + 2),
        ));
        let restart = item
            .restart
            .is_some()
            .then(|| theme::pill("restart", theme::PEACH));
        let pills_w = PILL_W + restart.as_ref().map_or(0, |p| p.width() + 1);
        let budget = width.saturating_sub(2 + key_w + 2 + pills_w + 1);
        let values = value_spans(item, budget);
        let shown: usize = values.iter().map(Span::width).sum();
        spans.extend(values);
        if let Some(new) = &item.restart {
            let text = match new {
                Some(v) => format!(" → {}", inline(v)),
                None => " → none".to_string(),
            };
            spans.push(Span::styled(
                clip(&text, budget.saturating_sub(shown)),
                Style::new().fg(theme::MUTED),
            ));
        }
        let mut pills = Vec::new();
        if let Some(r) = restart {
            pills.push(r);
            pills.push(Span::raw(" "));
        }
        pills.push(theme::pill(item.source.name(), source_color(item.source)));
        let pills_w: usize = pills.iter().map(Span::width).sum();
        let used: usize = spans.iter().map(Span::width).sum();
        if used + pills_w <= width {
            spans.push(Span::raw(" ".repeat(width - used - pills_w)));
            spans.extend(pills);
        }
        Line::from(spans)
    }

    fn expanded_lines(
        &self,
        item: &Item,
        indent: usize,
        width: usize,
        accent: Color,
    ) -> Vec<Line<'static>> {
        let Some(lines) = item.lines() else {
            return Vec::new();
        };
        let inner = width.saturating_sub(indent + PILL_W + 4);
        let bg = Style::new().bg(theme::RAISED);
        lines
            .iter()
            .map(|l| {
                let text = clip(&l.replace('\t', "    "), inner);
                let pad = inner.saturating_sub(text_width(&text));
                Line::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled("▎", bg.fg(accent)),
                    Span::styled(format!(" {text}{} ", " ".repeat(pad)), bg.fg(theme::TEXT)),
                ])
            })
            .collect()
    }

    fn body(&self, width: usize) -> Body {
        let rows = self.rows();
        let mut lines = Vec::new();
        let mut cursor = None;
        let mut key_w: BTreeMap<usize, usize> = BTreeMap::new();
        let cap = (width * 2 / 5).max(8);
        for r in &rows {
            if let Row::Item(s, i) = r {
                let w = text_width(&self.sections[*s].items[*i].key);
                let e = key_w.entry(*s).or_default();
                *e = (*e).max(w).min(cap);
            }
        }
        for (ri, r) in rows.iter().enumerate() {
            let here = ri == self.cursor;
            let start = lines.len();
            match *r {
                Row::Header(s) => {
                    if ri > 0 {
                        lines.push(Line::raw(""));
                    }
                    let start = lines.len();
                    lines.push(self.header_line(s, width, here));
                    if here {
                        cursor = Some((start, start));
                    }
                    continue;
                }
                Row::Item(s, i) => {
                    let item = &self.sections[s].items[i];
                    let kw = key_w.get(&s).copied().unwrap_or(1);
                    lines.push(self.item_line(item, kw, width, here));
                    if self.expanded.contains(&item.dotted()) {
                        lines.extend(self.expanded_lines(
                            item,
                            kw + 4,
                            width,
                            self.sections[s].accent,
                        ));
                    }
                }
            }
            if here {
                cursor = Some((start, lines.len() - 1));
            }
        }
        if let Some((c, _)) = cursor
            && let Some(l) = lines.get_mut(c)
        {
            *l = highlight(std::mem::take(l), width);
        }
        Body { lines, cursor }
    }

    fn header(&self, width: usize) -> Vec<Line<'static>> {
        let mut chips: Vec<Vec<Span<'static>>> = Vec::new();
        for f in &self.files {
            let (glyph, color, path_color) = match f.exists {
                Some(true) => ("●", theme::GREEN, theme::TEXT),
                Some(false) => ("○", theme::OVERLAY, theme::OVERLAY),
                None => ("○", theme::MUTED, theme::MUTED),
            };
            chips.push(vec![
                Span::styled(format!("{glyph} "), Style::new().fg(color)),
                Span::styled(
                    format!("{} ", f.source.name()),
                    Style::new().fg(source_color(f.source)).bold(),
                ),
                Span::styled(f.path.clone(), Style::new().fg(path_color)),
            ]);
        }
        if !self.flags.is_empty() {
            let color = source_color(Source::Flag);
            let mut chip = vec![Span::styled("flags", Style::new().fg(color).bold())];
            for (k, v) in &self.flags {
                chip.push(Span::raw(" "));
                chip.push(theme::pill(format!("{} = {}", key(k), inline(v)), color));
            }
            chips.push(chip);
        }
        if self.hide_defaults {
            chips.push(vec![theme::soft_pill("hiding defaults", theme::YELLOW)]);
        }
        if !self.filter.is_empty() && !self.typing {
            chips.push(vec![
                theme::soft_pill(format!("/{}", self.filter), theme::YELLOW),
                Span::styled(
                    format!(" {} match(es)", self.matches()),
                    Style::new().fg(theme::OVERLAY),
                ),
            ]);
        }
        let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
        if let Some(e) = &self.reload.error {
            for (i, l) in theme::wrap(
                &format!("reload failed: {e}"),
                width.saturating_sub(2).max(1),
            )
            .into_iter()
            .enumerate()
            {
                let glyph = if i == 0 { "✗ " } else { "  " };
                lines.push(vec![Span::styled(
                    format!("{glyph}{l}"),
                    Style::new().fg(theme::RED).bold(),
                )]);
            }
        }
        lines.push(Vec::new());
        for chip in chips {
            let w: usize = chip.iter().map(Span::width).sum();
            let line = lines.last_mut().expect("one line");
            let used: usize = line.iter().map(Span::width).sum();
            if used > 0 && used + 3 + w > width {
                lines.push(chip);
            } else {
                if used > 0 {
                    line.push(Span::raw("   "));
                }
                line.extend(chip);
            }
        }
        lines.into_iter().map(Line::from).collect()
    }
}

pub fn render(f: &mut Frame, outer: Rect, view: &ConfigView) {
    let w = ((outer.width as u32 * 9 / 10) as u16).min(110);
    let h = (outer.height as u32 * 9 / 10) as u16;
    let area = theme::centered(
        outer,
        w.max(outer.width.min(40)),
        h.max(outer.height.min(10)),
    );
    let inner = theme::popup(
        f,
        area,
        theme::BLUE,
        theme::popup_title("⚙", "Config", theme::BLUE),
    );
    let width = inner.width as usize;
    let header = view.header(width);
    let prompt = view.typing as u16;
    let [head, sep, body, foot] = Layout::vertical([
        Constraint::Length(header.len() as u16),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(prompt),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(header), head);
    f.render_widget(
        Paragraph::new(Line::styled(
            "─".repeat(width),
            Style::new().fg(theme::LINE),
        )),
        sep,
    );
    let Body { lines, cursor } = view.body(width);
    let height = body.height as usize;
    let total = lines.len();
    let mut off = view.offset.get();
    if let Some((start, end)) = cursor {
        if end >= off + height {
            off = (end + 1).saturating_sub(height);
        }
        if start < off {
            off = start;
        }
    }
    off = off.min(total.saturating_sub(height));
    view.offset.set(off);
    if lines.is_empty() {
        let why = if view.filter.is_empty() {
            "every key has its default value".to_string()
        } else {
            format!("no keys match /{}", view.filter)
        };
        f.render_widget(
            Paragraph::new(Line::styled(why, Style::new().fg(theme::MUTED).italic())),
            body,
        );
    } else {
        f.render_widget(
            Paragraph::new(lines.into_iter().skip(off).take(height).collect::<Vec<_>>()),
            body,
        );
    }
    if total > height && height > 0 && area.width > 0 {
        let track = Rect::new(area.right() - 1, body.y, 1, body.height);
        let mut state = ScrollbarState::new(total - height + 1).position(off);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(Style::new().fg(theme::LINE))
                .thumb_symbol("┃")
                .thumb_style(Style::new().fg(theme::BLUE)),
            track,
            &mut state,
        );
    }
    if view.typing {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("/", Style::new().fg(theme::YELLOW).bold()),
                Span::styled(view.filter.clone(), Style::new().fg(theme::TEXT)),
                Span::styled("▏", Style::new().fg(theme::YELLOW)),
                Span::styled(
                    format!("  {} match(es)", view.matches()),
                    Style::new().fg(theme::OVERLAY),
                ),
            ])),
            foot,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("sulla-cfgview-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        fn layers(&self, global: &str, repo: Option<&str>, flags: &str) -> Layers {
            let g = self.0.join("global.toml");
            let r = self.0.join("repo.toml");
            std::fs::write(&g, global).unwrap();
            if let Some(repo) = repo {
                std::fs::write(&r, repo).unwrap();
            }
            Layers::load_from(Some(&g), &r, toml::from_str(flags).unwrap()).unwrap()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const GLOBAL: &str = r#"
max_agents = 7
system_prompt = """
first rule
second rule
third rule
"""
max_agent = 4
[incus]
memory = "8GiB"
mounts = ["~/.claude", "~/.a-really-long-mount-path-that-will-not-fit-in-eighty-columns/at/all"]
"#;

    fn view(tag: &str, edit: impl FnOnce(&mut Config)) -> (ConfigView, Dir) {
        let d = Dir::new(tag);
        let layers = d.layers(GLOBAL, None, "base_branch = \"main\"");
        let mut cfg = layers.config().unwrap();
        edit(&mut cfg);
        (
            ConfigView::new(&layers, &cfg, &reload::State::default()).unwrap(),
            d,
        )
    }

    fn draw(v: &ConfigView, w: u16, h: u16) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, f.area(), v)).unwrap();
        t.backend().buffer().clone()
    }

    fn text(b: &Buffer) -> Vec<String> {
        (0..b.area.height)
            .map(|y| {
                (0..b.area.width)
                    .map(|x| b[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    fn line<'a>(screen: &'a [String], needle: &str) -> &'a str {
        screen
            .iter()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} in\n{}", screen.join("\n")))
    }

    fn find(v: &ConfigView, path: &str) -> Row {
        v.sections
            .iter()
            .enumerate()
            .find_map(|(s, sec)| {
                sec.items
                    .iter()
                    .position(|i| i.path.join(".") == path)
                    .map(|i| Row::Item(s, i))
            })
            .unwrap_or_else(|| panic!("no {path}"))
    }

    fn section(v: &ConfigView, name: &str) -> Row {
        Row::Header(v.sections.iter().position(|s| s.name == name).unwrap())
    }

    #[test]
    fn renders_sections_values_and_sources_at_80x24() {
        let (v, _d) = view("basic", |_| {});
        let b = draw(&v, 80, 24);
        let screen = text(&b);
        assert!(line(&screen, "⚙ Config").contains('╭'));
        assert!(line(&screen, "● global").contains("global.toml"));
        assert!(line(&screen, "○ repo").contains("repo.toml"));
        let flag = screen
            .iter()
            .position(|l| l.contains("base_branch = \"main\""))
            .unwrap();
        assert!(flag < screen.iter().position(|l| l.contains("General")).unwrap());
        assert!(!screen.iter().any(|l| l.contains("  hooks ")));
        assert!(line(&screen, "▾ General (").contains('─'));
        let max = line(&screen, "  max_agents ");
        assert!(max.contains(" 7 ") && max.contains(" global "), "{max}");
        let poll = line(&screen, "  poll_secs ");
        assert!(poll.contains(" default "), "{poll}");
        let kache = line(&screen, "  kache ");
        assert!(kache.contains("✓ true"), "{kache}");
        let prompt = line(&screen, "  system_prompt ");
        assert!(prompt.contains("first rule … (+2 lines)"), "{prompt}");
        let check = line(&screen, "  check ");
        assert!(check.contains("none"), "{check}");
        assert_eq!(
            source_cell_color(&b, "  max_agents ", "global"),
            Some(source_color(Source::Global))
        );
        for l in &screen {
            assert_eq!(text_width(l), 80, "{l}");
        }
        let end = |l: &str, tag: &str| l[..l.rfind(tag).unwrap() + tag.len()].chars().count();
        assert_eq!(end(poll, "default"), end(max, "global"));
    }

    #[test]
    fn reload_state_marks_restart_keys_flashes_changes_and_shows_errors() {
        let d = Dir::new("reload");
        let layers = d.layers("max_agents = 5\n[incus]\nmemory = \"4GiB\"", None, "");
        let mut cfg = layers.config().unwrap();
        cfg.incus.memory = "8GiB".into();
        let path = |s: &str| s.split('.').map(String::from).collect::<Vec<_>>();
        let state = reload::State {
            error: Some("repo.toml line 2, column 14: invalid type".into()),
            restart: [
                (
                    path("incus.memory"),
                    Some(toml::Value::String("4GiB".into())),
                ),
                (path("incus.env.A"), Some(toml::Value::String("1".into()))),
            ]
            .into_iter()
            .collect(),
            changed: [path("max_agents")].into_iter().collect(),
            at: Some(Instant::now()),
        };
        let mut v = ConfigView::new(&layers, &cfg, &state).unwrap();
        let memory = v.item(find(&v, "incus.memory")).unwrap().clone();
        assert_eq!(memory.source, Source::Global);
        assert_eq!(memory.value, Some(toml::Value::String("8GiB".into())));
        assert_eq!(
            memory.restart,
            Some(Some(toml::Value::String("4GiB".into())))
        );
        let env = v.item(find(&v, "incus.env.A")).unwrap().clone();
        assert_eq!(env.value, None);
        assert!(
            !v.sections
                .iter()
                .flat_map(|s| &s.items)
                .any(|i| i.dotted() == "incus.env")
        );
        assert!(env.restart.is_some());
        let max = v.item(find(&v, "max_agents")).unwrap().clone();
        assert!(v.flashing(&max));
        assert!(!v.flashing(&memory));

        let b = draw(&v, 100, 40);
        let screen = text(&b);
        assert!(
            line(&screen, "✗ reload failed: repo.toml line 2, column 14").contains("invalid type")
        );
        let y = screen
            .iter()
            .position(|l| l.contains("  max_agents "))
            .unwrap();
        let x = screen[y][..screen[y].find("max_agents").unwrap()]
            .chars()
            .count();
        assert_eq!(b[(x as u16, y as u16)].fg, theme::YELLOW);
        let y = screen.iter().position(|l| l.contains("  kache ")).unwrap();
        let x = screen[y][..screen[y].find("kache").unwrap()]
            .chars()
            .count();
        assert_eq!(b[(x as u16, y as u16)].fg, theme::TEXT);

        v.set_filter("memory".into());
        let b = draw(&v, 100, 40);
        let screen = text(&b);
        let row = line(&screen, "  memory ");
        assert!(row.contains("\"8GiB\" → \"4GiB\""), "{row}");
        assert!(row.contains(" restart   global "), "{row}");
        let y = screen.iter().position(|l| l.contains(" restart ")).unwrap();
        let x = screen[y][..screen[y].find(" restart ").unwrap()]
            .chars()
            .count()
            + 1;
        assert_eq!(b[(x as u16, y as u16)].bg, theme::PEACH);
        v.fold_all(true);
        let mut fresh = ConfigView::new(&layers, &cfg, &reload::State::default()).unwrap();
        fresh.keep_ui(&v);
        assert_eq!(fresh.filter, "memory");
        let names = |v: &ConfigView| {
            v.rows()
                .into_iter()
                .map(|r| match r {
                    Row::Header(s) => v.sections[s].name.clone(),
                    r => v.item(r).unwrap().dotted(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&fresh), names(&v));
        fresh.set_filter(String::new());
        assert!(fresh.rows().iter().all(|r| matches!(r, Row::Header(_))));
        assert!(
            !text(&draw(&fresh, 100, 40))
                .iter()
                .any(|l| l.contains("reload failed"))
        );
    }

    fn source_cell_color(b: &Buffer, needle: &str, tag: &str) -> Option<Color> {
        let screen = text(b);
        let y = screen.iter().position(|l| l.contains(needle))?;
        let row = &screen[y];
        let x = row[..row.rfind(tag)?].chars().count();
        Some(b[(x as u16, y as u16)].bg)
    }

    #[test]
    fn long_values_truncate_without_wrapping() {
        let (mut v, _d) = view("long", |_| {});
        v.set_filter("incus.mounts".into());
        let screen = text(&draw(&v, 80, 24));
        let mounts = line(&screen, "  mounts ");
        assert!(mounts.contains("[~/.claude, ~/.a-really"), "{mounts}");
        assert!(mounts.contains('…'), "{mounts}");
        assert!(mounts.contains(" global "), "{mounts}");
        assert_eq!(
            screen.iter().filter(|l| l.contains("eighty")).count(),
            0,
            "{}",
            screen.join("\n")
        );
    }

    #[test]
    fn folded_tables_hide_their_rows() {
        let (mut v, _d) = view("fold", |_| {});
        v.fold_all(true);
        let screen = text(&draw(&v, 100, 40));
        assert!(line(&screen, "▸ incus (").contains('─'));
        assert!(!screen.iter().any(|l| l.contains("  memory ")));
        assert!(v.select(section(&v, "incus")));
        assert!(v.toggle());
        let screen = text(&draw(&v, 100, 40));
        assert!(line(&screen, "▾ incus (").contains('─'));
        assert!(line(&screen, "  memory ").contains("\"8GiB\""));
        assert!(line(&screen, "▸ General (").contains('─'));
        v.fold_all(false);
        let screen = text(&draw(&v, 100, 60));
        assert!(line(&screen, "▾ General (").contains('─'));
    }

    #[test]
    fn runtime_changes_show_the_runtime_source() {
        let (v, _d) = view("runtime", |c| c.max_agents = 5);
        let max = v.item(find(&v, "max_agents")).unwrap();
        assert_eq!(max.source, Source::Runtime);
        assert_eq!(max.value, Some(toml::Value::Integer(5)));
        assert_eq!(
            v.item(find(&v, "base_branch")).unwrap().source,
            Source::Flag
        );
        let b = draw(&v, 80, 24);
        let screen = text(&b);
        let row = line(&screen, "  max_agents ");
        assert!(row.contains(" 5 ") && row.contains(" runtime "), "{row}");
        assert_eq!(
            source_cell_color(&b, "  max_agents ", "runtime"),
            Some(theme::YELLOW)
        );
    }

    #[test]
    fn hide_defaults_and_filter_narrow_the_rows() {
        let (mut v, _d) = view("narrow", |_| {});
        v.toggle_defaults();
        let screen = text(&draw(&v, 100, 40));
        let hiding = screen
            .iter()
            .position(|l| l.contains("hiding defaults"))
            .unwrap();
        assert!(hiding < screen.iter().position(|l| l.contains("General")).unwrap());
        assert!(!screen.iter().any(|l| l.contains("  poll_secs ")));
        assert!(line(&screen, "  memory ").contains("global"));
        v.toggle_defaults();
        v.set_filter("MEM".into());
        assert_eq!(v.current(), Some(find(&v, "incus.memory")));
        let b = draw(&v, 100, 40);
        let screen = text(&b);
        assert!(line(&screen, "/MEM").contains("1 match(es)"));
        assert!(!screen.iter().any(|l| l.contains("max_agents")));
        let y = screen.iter().position(|l| l.contains("  memory ")).unwrap() as u16;
        let x = screen[y as usize].find("memory").unwrap();
        let x = screen[y as usize][..x].chars().count() as u16;
        assert_eq!(b[(x, y)].bg, theme::YELLOW);
        assert_eq!(b[(x + 3, y)].bg, theme::CURSOR_BG);
        v.set_filter("nothing-here".into());
        let screen = text(&draw(&v, 100, 40));
        assert!(line(&screen, "no keys match /nothing-here").contains('│'));
    }

    #[test]
    fn multi_line_values_expand_inline() {
        let (mut v, _d) = view("expand", |_| {});
        assert!(v.select(find(&v, "system_prompt")));
        assert!(v.toggle());
        let b = draw(&v, 100, 40);
        let screen = text(&b);
        let y = screen
            .iter()
            .position(|l| l.contains("▎ second rule"))
            .unwrap();
        assert!(screen[y - 1].contains("▎ first rule"));
        assert!(screen[y + 1].contains("▎ third rule"));
        let x = screen[y][..screen[y].find("second").unwrap()]
            .chars()
            .count() as u16;
        assert_eq!(b[(x, y as u16)].bg, theme::RAISED);
        assert!(v.toggle());
        assert!(
            !text(&draw(&v, 100, 40))
                .iter()
                .any(|l| l.contains("second rule"))
        );
        assert!(v.select(find(&v, "max_agents")));
        assert!(!v.toggle());
    }

    #[test]
    fn unknown_keys_get_a_red_section_with_their_file() {
        let (mut v, _d) = view("unknown", |_| {});
        let last = v.sections.last().unwrap();
        assert!(last.unknown);
        assert_eq!(last.accent, theme::RED);
        v.bottom();
        let screen = text(&draw(&v, 100, 30));
        assert!(line(&screen, "▾ Unknown keys (1)").contains('─'));
        let row = line(&screen, "  max_agent ");
        assert!(row.contains("4  ") && row.contains("global.toml"), "{row}");
    }

    #[test]
    fn navigation_jumps_tables_and_yanks_values() {
        let (mut v, _d) = view("nav", |_| {});
        assert_eq!(v.current(), Some(section(&v, GENERAL)));
        v.next_section(true);
        assert_eq!(v.current(), Some(section(&v, "ai_review")));
        v.next_section(false);
        assert_eq!(v.current(), Some(section(&v, GENERAL)));
        assert!(v.select(find(&v, "incus.memory")));
        assert_eq!(v.yank().as_deref(), Some("incus.memory = \"8GiB\""));
        assert!(v.select(find(&v, "system_prompt")));
        assert_eq!(
            v.yank().as_deref(),
            Some("system_prompt = \"first rule\\nsecond rule\\nthird rule\\n\"")
        );
        assert!(v.select(find(&v, "check")));
        assert_eq!(v.yank().as_deref(), Some("check is unset"));
        v.toggle_fold();
        assert_eq!(v.current(), Some(section(&v, GENERAL)));
        v.bottom();
        v.move_by(5);
        assert_eq!(v.cursor, v.rows().len() - 1);
        v.top();
        v.move_by(-3);
        assert_eq!(v.cursor, 0);
    }

    #[test]
    fn renders_at_tiny_sizes() {
        let (mut v, _d) = view("tiny", |_| {});
        v.typing = true;
        v.filter = "a".into();
        for (w, h) in [(24, 8), (6, 3), (1, 1)] {
            draw(&v, w, h);
        }
    }
}
