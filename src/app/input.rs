use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::agent::{self, CHAT_ID};
use crate::beads::{Column, State};
use crate::filter::Filter;
use crate::presets;
use crate::ui::keys;

use super::{App, InputKind, Mode, NewBeadForm, View};

const MAX_COUNT: usize = 99_999;

impl App {
    pub(super) fn on_paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Insert => {
                let (_, bracketed) = self.focused_app_cursor();
                self.write_focused(&keys::paste(text, bracketed));
            }
            Mode::Input { buf, .. } => buf.push_str(&text.replace(['\r', '\n'], " ")),
            Mode::NewBead(form) => match form.field {
                0 => form.title.push_str(&text.replace(['\r', '\n'], " ")),
                1 => form.description.push_str(&text.replace('\r', "")),
                _ => {}
            },
            _ => {}
        }
    }

    pub(super) fn on_mouse(&mut self, m: MouseEvent) {
        let up = match m.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        if let View::Progress(id) = &self.view
            && matches!(self.mode, Mode::Normal | Mode::Insert)
        {
            let (rows, cols) = self.pty_size();
            let col = m.column.saturating_sub(1).min(cols - 1);
            let row = m.row.saturating_sub(1).min(rows - 1);
            if let Some(a) = self.agents.get_mut(id) {
                a.scroll_at(if up { 3 } else { -3 }, col, row);
            }
            return;
        }
        if matches!(
            self.mode,
            Mode::Normal | Mode::Inspect { .. } | Mode::Help { .. }
        ) {
            let code = if up { KeyCode::Up } else { KeyCode::Down };
            self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    pub(super) fn on_key(&mut self, k: KeyEvent) {
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Insert => self.insert_key(k),
            Mode::Input { kind, buf } => self.input_key(k, kind, buf),
            Mode::NewBead(form) => self.new_bead_key(k, form),
            Mode::Confirm { action, .. } => {
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.confirm(action);
                } else {
                    self.status = "cancelled".into();
                }
            }
            Mode::Help { scroll } => self.help_key(k, scroll),
            Mode::Health => {
                if k.code == KeyCode::Char('r') {
                    self.open_health();
                }
            }
            Mode::Inspect { id, text, scroll } => self.inspect_key(k, id, text, scroll),
            Mode::Normal => self.normal_key(k),
        }
    }

    fn insert_key(&mut self, k: KeyEvent) {
        self.mode = Mode::Insert;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5')) {
            self.esc_at = None;
            self.mode = Mode::Normal;
            return;
        }
        if k.code == KeyCode::Esc && k.modifiers == KeyModifiers::ALT {
            self.esc_at = None;
            self.mode = Mode::Normal;
            return;
        }
        if k.code == KeyCode::Esc && k.modifiers.is_empty() {
            if self.esc_at.take().is_some() {
                self.mode = Mode::Normal;
            } else {
                self.esc_at = Some(Instant::now());
            }
            return;
        }
        if self.esc_at.take().is_some() {
            self.write_focused(&[0x1b]);
        }
        let (app_cursor, _) = self.focused_app_cursor();
        self.write_focused(&keys::encode(k, app_cursor));
    }

    fn input_key(&mut self, k: KeyEvent, kind: InputKind, mut buf: String) {
        if let InputKind::Search(prev) = kind {
            return self.search_key(k, prev, buf);
        }
        match k.code {
            KeyCode::Esc => {}
            KeyCode::Enter => self.submit(kind, buf.trim().to_string()),
            KeyCode::Backspace => {
                if buf.pop().is_some() || !matches!(kind, InputKind::Command) {
                    self.mode = Mode::Input { kind, buf };
                }
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mode = Mode::Input {
                    kind,
                    buf: String::new(),
                };
            }
            KeyCode::Char(c) => {
                buf.push(c);
                self.mode = Mode::Input { kind, buf };
            }
            _ => self.mode = Mode::Input { kind, buf },
        }
    }

    fn search_key(&mut self, k: KeyEvent, prev: String, mut buf: String) {
        match k.code {
            KeyCode::Esc => return self.set_filter(&prev),
            KeyCode::Enter => {
                self.set_filter(buf.trim());
                return self.focus_match();
            }
            KeyCode::Backspace if buf.pop().is_none() => return self.set_filter(""),
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => buf.clear(),
            KeyCode::Char(c) => buf.push(c),
            _ => {}
        }
        self.set_filter(&buf);
        self.mode = Mode::Input {
            kind: InputKind::Search(prev),
            buf,
        };
    }

    fn set_filter(&mut self, query: &str) {
        let filter = Filter::parse(query);
        self.refilter(|app| app.filter = filter);
    }

    fn toggle_chain_filter(&mut self) {
        if let Some(id) = self.chain_filter.clone() {
            self.refilter(|app| app.chain_filter = None);
            self.status = format!("chain filter for {id} cleared");
            return;
        }
        let Some(b) = self.selected() else {
            self.status = "no bead selected".into();
            return;
        };
        let id = b.id;
        self.refilter(|app| app.chain_filter = Some(id.clone()));
        let n: usize = self.columns().iter().map(Vec::len).sum();
        self.status = format!("chain of {id}: {n} bead(s), L clears");
    }

    fn refilter(&mut self, apply: impl FnOnce(&mut Self)) {
        let cols = self.columns();
        let selected: Vec<Option<String>> = cols
            .iter()
            .zip(self.rows)
            .map(|(c, r)| c.get(r).map(|b| b.id.clone()))
            .collect();
        apply(self);
        let rows: Vec<usize> = self
            .columns()
            .iter()
            .zip(&selected)
            .map(|(c, sel)| {
                sel.as_ref()
                    .and_then(|id| c.iter().position(|b| b.id == *id))
                    .unwrap_or(0)
            })
            .collect();
        self.rows.copy_from_slice(&rows);
        self.clamp();
    }

    fn focus_match(&mut self) {
        if !self.filter.is_active() {
            return;
        }
        let cols = self.columns();
        let n: usize = cols.iter().map(Vec::len).sum();
        if cols[self.col].is_empty()
            && let Some(i) = cols.iter().position(|c| !c.is_empty())
        {
            self.col = i;
        }
        self.status = format!("{n} match(es) for /{}, Esc clears", self.filter.query);
    }

    fn new_bead_key(&mut self, k: KeyEvent, mut form: NewBeadForm) {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => {
                self.status = "cancelled".into();
                return;
            }
            KeyCode::Enter if ctrl || shift || form.field >= 2 || form.field == 0 => {
                let title = form.title.trim().to_string();
                if title.is_empty() {
                    self.status = "title required".into();
                    form.field = 0;
                } else {
                    let labels = form.labels();
                    self.create_bead(
                        &title,
                        form.description.trim(),
                        form.priority,
                        form.backlog,
                        &labels,
                    );
                    return;
                }
            }
            KeyCode::Enter => form.field = 2,
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % NewBeadForm::FIELDS,
            KeyCode::BackTab | KeyCode::Up => {
                form.field = (form.field + NewBeadForm::FIELDS - 1) % NewBeadForm::FIELDS
            }
            KeyCode::Right | KeyCode::Char('-') if form.field == 2 => {
                form.priority = (form.priority + 1).min(4)
            }
            KeyCode::Left | KeyCode::Char('+') | KeyCode::Char('=') if form.field == 2 => {
                form.priority = form.priority.saturating_sub(1)
            }
            KeyCode::Char(c @ '0'..='4') if form.field == 2 => form.priority = c as u8 - b'0',
            KeyCode::Left if form.field == 3 => form.cycle_agent(false),
            KeyCode::Right | KeyCode::Char(' ') if form.field == 3 => form.cycle_agent(true),
            KeyCode::Left if form.field == 4 => form.cycle_preset(false),
            KeyCode::Right | KeyCode::Char(' ') if form.field == 4 => form.cycle_preset(true),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 5 => {
                form.backlog = !form.backlog
            }
            KeyCode::Char('b') if form.field == 5 => form.backlog = true,
            KeyCode::Char('p') if form.field == 5 => form.backlog = false,
            KeyCode::Backspace => {
                match form.field {
                    0 => form.title.pop(),
                    1 => form.description.pop(),
                    _ => None,
                };
            }
            KeyCode::Char('u') if ctrl => match form.field {
                0 => form.title.clear(),
                1 => form.description.clear(),
                _ => {}
            },
            KeyCode::Char(c) if !ctrl => match form.field {
                0 => form.title.push(c),
                1 => form.description.push(c),
                _ => {}
            },
            _ => {}
        }
        self.mode = Mode::NewBead(form);
    }

    pub(super) fn submit(&mut self, kind: InputKind, text: String) {
        if text.is_empty() {
            return;
        }
        match kind {
            InputKind::Feedback(id) => self.feedback(&id, &text),
            InputKind::Retry(id) => self.retry(&id, &text),
            InputKind::Command => self.command(&text),
            InputKind::Search(_) => self.set_filter(&text),
            InputKind::Comment(count) => {
                if let View::Review(d) = &mut self.view {
                    if d.add_comment(&text, count) {
                        self.status = format!("{} comment(s), S to send", d.comments.len());
                    } else {
                        self.status = "cannot comment on this line".into();
                    }
                }
            }
            InputKind::EditComment(i) => {
                if let View::Review(d) = &mut self.view
                    && let Some(c) = d.comments.get_mut(i)
                {
                    c.text = text;
                    self.status = "comment updated".into();
                }
                self.sync_ai();
            }
        }
    }

    pub(super) fn command(&mut self, cmd: &str) {
        let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let rest = rest.trim();
        match name {
            "q" | "quit" => self.request_quit(),
            "q!" | "quit!" => self.quit = true,
            "new" | "n" if !rest.is_empty() => self.create_bead(rest, "", 2, false, &[]),
            "max" => match rest.parse() {
                Ok(n) => {
                    self.cfg.max_agents = n;
                    self.status = format!("max agents = {n}");
                    self.schedule();
                }
                Err(_) => self.status = format!("max agents = {}", self.cfg.max_agents),
            },
            "pause" => self.set_paused(true),
            "resume" => self.set_paused(false),
            "refresh" | "r" => {
                self.failed.clear();
                self.mutated();
            }
            "kill" if !rest.is_empty() => self.confirm_kill(rest.to_string()),
            "chat" => self.open_chat(rest == "new"),
            "health" => self.open_health(),
            _ => self.status = format!("unknown command: {cmd}"),
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        let pending = self.pending.take();
        let count = self.count.take();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('?') => return self.mode = Mode::Help { scroll: 0 },
            KeyCode::Char(':') => {
                return self.mode = Mode::Input {
                    kind: InputKind::Command,
                    buf: String::new(),
                };
            }
            KeyCode::Char('c') if ctrl => return self.request_quit(),
            _ => {}
        }
        match &self.view {
            View::Board => self.board_key(k, pending),
            View::Progress(_) => self.progress_key(k, pending),
            View::Review(_) => self.review_key(k, pending, count),
        }
    }

    fn board_key(&mut self, k: KeyEvent, pending: Option<char>) {
        let len = self.columns()[self.col].len();
        let row = &mut self.rows[self.col];
        match k.code {
            KeyCode::Char('h') | KeyCode::Left => self.col = self.col.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => {
                self.col = (self.col + 1).min(Column::ALL.len() - 1)
            }
            KeyCode::Char('j') | KeyCode::Down => *row = (*row + 1).min(len.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *row = row.saturating_sub(1),
            KeyCode::Char('g') if pending == Some('g') => *row = 0,
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') => *row = len.saturating_sub(1),
            KeyCode::Char('d') if pending == Some('d') => {
                if let Some(b) = self.selected() {
                    self.confirm_delete(b.id);
                }
            }
            KeyCode::Char('d') => self.pending = Some('d'),
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Esc if self.zoom => self.zoom = false,
            KeyCode::Esc if self.filter.is_active() => {
                self.set_filter("");
                self.status = "filter cleared".into();
            }
            KeyCode::Esc if self.chain_filter.is_some() => self.toggle_chain_filter(),
            KeyCode::Char('L') => self.toggle_chain_filter(),
            KeyCode::Char('/') => {
                let q = self.filter.query.clone();
                self.mode = Mode::Input {
                    kind: InputKind::Search(q.clone()),
                    buf: q,
                };
            }
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('n') => {
                self.mode = Mode::NewBead(NewBeadForm::new(presets::names(&self.cfg.presets)));
            }
            KeyCode::Char('c') => self.open_chat(false),
            KeyCode::Char('C') => self.open_chat(true),
            KeyCode::Char('P') => self.set_paused(!self.paused),
            KeyCode::Char('!') => self.open_health(),
            _ => self.board_bead_key(k),
        }
    }

    fn help_key(&mut self, k: KeyEvent, mut scroll: u16) {
        let pending = self.pending.take();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = self.term.1 / 2;
        let max = crate::ui::help_max_scroll(self.term.1);
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => scroll = scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => scroll = scroll.saturating_add(half),
            KeyCode::Char('u') if ctrl => scroll = scroll.saturating_sub(half),
            KeyCode::Char('g') if pending == Some('g') => scroll = 0,
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') => scroll = max,
            _ => return,
        }
        self.mode = Mode::Help {
            scroll: scroll.min(max),
        };
    }

    fn inspect_key(&mut self, k: KeyEvent, id: String, text: String, mut scroll: u16) {
        let pending = self.pending.take();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = self.term.1 / 2;
        let max = text.lines().count().saturating_sub(1) as u16;
        match k.code {
            KeyCode::Char('q') | KeyCode::Char('i') | KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => scroll = scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => scroll = scroll.saturating_add(half),
            KeyCode::Char('u') if ctrl => scroll = scroll.saturating_sub(half),
            KeyCode::Char('g') if pending == Some('g') => scroll = 0,
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') => scroll = max,
            KeyCode::Char('p') => return self.open_progress(self.agent_key(&id)),
            KeyCode::Char('r') => return self.open_review(id),
            KeyCode::Char('l') => return self.show_log(id),
            KeyCode::Char('h') => {
                self.mode = Mode::Inspect {
                    id: id.clone(),
                    text,
                    scroll,
                };
                return self.show_timeline(id);
            }
            _ => {}
        }
        self.mode = Mode::Inspect {
            id,
            text,
            scroll: scroll.min(max),
        };
    }

    fn board_bead_key(&mut self, k: KeyEvent) {
        let Some(b) = self.selected() else { return };
        self.parked_review = None;
        match k.code {
            KeyCode::Enter => match Column::ALL[self.col] {
                Column::AwaitingPr
                    if matches!(b.state(), Some(State::Running | State::Reviewing)) =>
                {
                    self.open_progress(self.agent_key(&b.id))
                }
                Column::Review | Column::AwaitingPr | Column::Done => self.open_review(b.id),
                _ => self.open_progress(self.agent_key(&b.id)),
            },
            KeyCode::Char('i') => self.inspect(b.id),
            KeyCode::Char('H') => self.show_timeline(b.id),
            KeyCode::Char('e') => self.request_edit(&b.id),
            KeyCode::Char('E') => self.request_shell(&b.id),
            KeyCode::Char('p') => self.open_progress(self.agent_key(&b.id)),
            KeyCode::Char('V') => self.start_review(&b.id),
            KeyCode::Char('r') => self.open_review(b.id),
            KeyCode::Char('X') => self.confirm_kill(b.id),
            KeyCode::Char('D') if Column::ALL[self.col] != Column::Done => {
                self.confirm_discard(b.id)
            }
            KeyCode::Char('F') if Column::ALL[self.col] != Column::Done => self.request_retry(b.id),
            KeyCode::Char('R') => self.mark_review(&b.id),
            KeyCode::Char('s') => self.release(&b),
            KeyCode::Char('S') => self.release_backlog(),
            KeyCode::Char('B') => self.move_to_backlog(&b),
            KeyCode::Char('A') => self.relaunch(&b.id),
            KeyCode::Char('U') => self.request_revise(&b),
            KeyCode::Char('t') => self.show_check(b.id),
            KeyCode::Char('T') => self.rerun_check(&b.id),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.set_priority(&b.id, b.priority.saturating_sub(1))
            }
            KeyCode::Char('-') => self.set_priority(&b.id, (b.priority + 1).min(4)),
            _ => {}
        }
    }

    fn progress_key(&mut self, k: KeyEvent, pending: Option<char>) {
        let Some(id) = self.focused_id() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.pty_size().0 / 2) as isize;
        let chat = id == CHAT_ID;
        let bead = agent::reviewed_bead(&id).unwrap_or(&id).to_string();
        let agent = self.agents.get_mut(&id);
        match (k.code, agent) {
            (KeyCode::Char('R' | 'r'), _) if chat => {}
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                if !self.back_to_review(id) {
                    self.view = View::Board;
                }
            }
            (KeyCode::Char('i') | KeyCode::Char('a'), _) => self.enter_insert(&id),
            (KeyCode::Char('u'), Some(a)) if ctrl => a.scroll(half),
            (KeyCode::Char('d'), Some(a)) if ctrl => a.scroll(-half),
            (KeyCode::Char('k') | KeyCode::Up, Some(a)) => a.scroll(1),
            (KeyCode::Char('j') | KeyCode::Down, Some(a)) => a.scroll(-1),
            (KeyCode::Char('g'), Some(a)) if pending == Some('g') => a.scroll(isize::MAX / 2),
            (KeyCode::Char('g'), _) => self.pending = Some('g'),
            (KeyCode::Char('G'), Some(a)) => a.scroll(-(isize::MAX / 2)),
            (KeyCode::Char('X'), _) => self.confirm_kill(id),
            (KeyCode::Char('R'), _) => self.mark_review(&bead),
            (KeyCode::Char('r'), _) => {
                if !self.back_to_review(bead.clone()) {
                    self.open_review(bead);
                }
            }
            (KeyCode::Char('A'), _) => self.relaunch(&id),
            (KeyCode::Char('e'), _) => self.request_edit(&bead),
            (KeyCode::Char('E'), _) => self.request_shell(&bead),
            _ => {}
        }
    }

    fn review_key(&mut self, k: KeyEvent, pending: Option<char>, count: Option<usize>) {
        self.review_key_inner(k, pending, count);
        self.sync_ai();
    }

    fn review_key_inner(&mut self, k: KeyEvent, pending: Option<char>, count: Option<usize>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.pty_size().0 / 2) as isize;
        let View::Review(d) = &mut self.view else {
            return;
        };
        let id = d.id.clone();
        let n = count.unwrap_or(1);
        match k.code {
            KeyCode::Char(c @ '0'..='9') if c != '0' || count.is_some() => {
                let digit = c.to_digit(10).unwrap_or(0) as usize;
                self.count = Some((count.unwrap_or(0) * 10 + digit).min(MAX_COUNT));
            }
            KeyCode::Esc if count.is_some() => {}
            KeyCode::Char('q') | KeyCode::Esc => self.view = View::Board,
            KeyCode::Char('j') | KeyCode::Down => d.move_cursor(n as isize),
            KeyCode::Char('k') | KeyCode::Up => d.move_cursor(-(n as isize)),
            KeyCode::Char('d') if ctrl => d.move_cursor(half),
            KeyCode::Char('u') if ctrl => d.move_cursor(-half),
            KeyCode::Char('g') if pending == Some('g') => d.set_cursor(0),
            KeyCode::Char('G') => d.set_cursor(usize::MAX),
            KeyCode::Tab | KeyCode::Char('J') => d.next_file(),
            KeyCode::BackTab | KeyCode::Char('K') => d.prev_file(),
            KeyCode::Char('f') if pending == Some(']') => d.next_file(),
            KeyCode::Char('f') if pending == Some('[') => d.prev_file(),
            KeyCode::Char(c @ ('g' | ']' | '[')) => self.pending = Some(c),
            KeyCode::Char('c') => {
                if d.comment_range(n).is_some() {
                    self.mode = Mode::Input {
                        kind: InputKind::Comment(n),
                        buf: String::new(),
                    };
                } else {
                    self.status = "cannot comment on this line".into();
                }
            }
            KeyCode::Char('C') => match d.comment_at_cursor() {
                Some(i) => {
                    self.mode = Mode::Input {
                        kind: InputKind::EditComment(i),
                        buf: d.comments[i].text.clone(),
                    };
                }
                None => self.status = "no editable comment on this line".into(),
            },
            KeyCode::Char('x') => {
                let n = d.delete_comments_at_cursor();
                self.status = format!("deleted {n} comment(s)");
            }
            KeyCode::Char('S') => match d.feedback() {
                Some(text) => self.feedback(&id, &text),
                None => self.status = "no comments to send".into(),
            },
            KeyCode::Char('m') => self.request_merge(id, false),
            KeyCode::Char('M') => self.request_merge(id, true),
            KeyCode::Char('P') => self.request_publish(id),
            KeyCode::Char('t') => self.show_check(id),
            KeyCode::Char('T') => self.rerun_check(&id),
            KeyCode::Char('s') => {
                self.mode = Mode::Input {
                    kind: InputKind::Feedback(id),
                    buf: String::new(),
                };
            }
            KeyCode::Char('D') => self.confirm_discard(id),
            KeyCode::Char('F') => self.request_retry(id),
            KeyCode::Char('V') => self.start_review(&id),
            KeyCode::Char('p') | KeyCode::Char('a') => self.swap_to_agent(false),
            KeyCode::Char('i') => self.swap_to_agent(true),
            KeyCode::Char('R') => self.reload_review(id),
            KeyCode::Char('e') => self.request_edit(&id),
            KeyCode::Char('E') => self.request_shell(&id),
            KeyCode::Char(c @ ('y' | 'n' | 'Y' | 'N')) => {
                let accept = c.eq_ignore_ascii_case(&'y');
                let ok = if c.is_ascii_uppercase() {
                    d.mark_file(accept)
                } else {
                    d.mark_hunk(accept)
                };
                self.status = if ok {
                    format!("{} hunk(s) rejected", d.rejected_hunks())
                } else {
                    "commit messages cannot be rejected".into()
                };
            }
            _ => {}
        }
    }
}
