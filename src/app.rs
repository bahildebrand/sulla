use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use chrono::Utc;
use color_eyre::Result;
use crossterm::event::{Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::agent::{Agent, Backend, Claude, Copilot, Launch};
use crate::beads::{Bead, Beads, Column, State};
use crate::config::{AgentKind, Config};
use crate::git::{Git, RebaseConflict};
use crate::scheduler::{self, Snapshot};
use crate::ui::keys;
use crate::ui::review::Diff;

pub enum Event {
    Input(CEvent),
    Tick,
    AgentOutput,
    AgentExited(String),
    Snapshot(Result<Snapshot, String>),
}

pub enum View {
    Board,
    Progress(String),
    Review(Diff),
}

pub enum InputKind {
    Command,
    Feedback(String),
    Comment,
}

pub struct NewBeadForm {
    pub title: String,
    pub description: String,
    pub priority: u8,
    pub backlog: bool,
    pub field: usize,
}

impl Default for NewBeadForm {
    fn default() -> Self {
        Self {
            title: String::new(),
            description: String::new(),
            priority: 2,
            backlog: false,
            field: 0,
        }
    }
}

pub enum Action {
    Kill(String),
    Merge(String),
    ResolveConflicts(String),
    Discard(String),
    Quit,
}

pub enum Mode {
    Normal,
    Insert,
    Input {
        kind: InputKind,
        buf: String,
    },
    NewBead(NewBeadForm),
    Confirm {
        action: Action,
        msg: String,
    },
    Help,
    Inspect {
        id: String,
        text: String,
        scroll: u16,
    },
}

const ESC_TIMEOUT: Duration = Duration::from_millis(300);

pub struct App {
    pub cfg: Config,
    pub base: String,
    beads: Beads,
    git: Git,
    backend: Box<dyn Backend>,
    pub all: Vec<Bead>,
    ready: Vec<Bead>,
    pub agents: HashMap<String, Agent>,
    failed: HashSet<String>,
    pub view: View,
    pub parked_review: Option<Diff>,
    pub mode: Mode,
    pub col: usize,
    pub rows: [usize; Column::ALL.len()],
    pub status: String,
    pending: Option<char>,
    esc_at: Option<Instant>,
    last_mutation: Instant,
    tx: Sender<Event>,
    refresh: Sender<()>,
    term: (u16, u16),
    pub quit: bool,
}

impl App {
    pub fn new(cfg: Config, repo: PathBuf, tx: Sender<Event>) -> Result<Self> {
        let git = Git::new(repo.clone());
        git.ensure_ignored()?;
        let base = match &cfg.base_branch {
            Some(b) => b.clone(),
            None => git.current_branch()?,
        };
        let beads = Beads::new(&repo);
        let refresh = scheduler::spawn_poller(
            beads.clone(),
            Duration::from_secs(cfg.poll_secs.max(1)),
            tx.clone(),
        );
        let backend: Box<dyn Backend> = match cfg.agent {
            AgentKind::Copilot => Box::new(Copilot {
                cfg: cfg.copilot.clone(),
                repo,
            }),
            AgentKind::Claude => Box::new(Claude {
                cfg: cfg.claude.clone(),
                repo,
            }),
        };
        Ok(Self {
            cfg,
            base,
            beads,
            git,
            backend,
            all: Vec::new(),
            ready: Vec::new(),
            agents: HashMap::new(),
            failed: HashSet::new(),
            view: View::Board,
            parked_review: None,
            mode: Mode::Normal,
            col: 0,
            rows: [0; Column::ALL.len()],
            status: String::new(),
            pending: None,
            esc_at: None,
            last_mutation: Instant::now(),
            tx,
            refresh,
            term: (80, 24),
            quit: false,
        })
    }

    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    pub fn columns(&self) -> [Vec<&Bead>; Column::ALL.len()] {
        let now = Utc::now();
        let mut cols: [Vec<&Bead>; Column::ALL.len()] = Default::default();
        for b in &self.all {
            if let Some(c) = b.column(now, self.cfg.done_retention_days) {
                cols[Column::ALL.iter().position(|x| *x == c).unwrap()].push(b);
            }
        }
        for (i, c) in cols.iter_mut().enumerate() {
            if Column::ALL[i] == Column::Done {
                c.sort_by_key(|b| std::cmp::Reverse(b.closed_at));
            } else {
                c.sort_by(|a, b| {
                    a.priority
                        .cmp(&b.priority)
                        .then(a.created_at.cmp(&b.created_at))
                });
            }
        }
        cols
    }

    fn selected(&self) -> Option<Bead> {
        self.columns()[self.col]
            .get(self.rows[self.col])
            .map(|b| (*b).clone())
    }

    fn bead(&self, id: &str) -> Option<&Bead> {
        self.all.iter().find(|b| b.id == id)
    }

    fn focused_id(&self) -> Option<String> {
        match &self.view {
            View::Board => self.selected().map(|b| b.id),
            View::Progress(id) => Some(id.clone()),
            View::Review(d) => Some(d.id.clone()),
        }
    }

    pub fn running_count(&self) -> usize {
        let now = Utc::now();
        self.agents
            .iter()
            .filter(|(_, a)| a.alive)
            .filter(|(id, _)| {
                !matches!(
                    self.bead(id).and_then(|b| b.column(now, i64::MAX)),
                    Some(Column::Review | Column::Done)
                )
            })
            .count()
    }

    pub fn pty_size(&self) -> (u16, u16) {
        (
            self.term.1.saturating_sub(3).max(1),
            self.term.0.saturating_sub(2).max(1),
        )
    }

    fn mutated(&mut self) {
        self.last_mutation = Instant::now();
        let _ = self.refresh.send(());
    }

    fn report<T>(&mut self, r: Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("{e:#}");
                self.status = format!("error: {e}");
                None
            }
        }
    }

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Input(CEvent::Key(k)) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Input(CEvent::Paste(s)) => self.on_paste(&s),
            Event::Input(CEvent::Resize(w, h)) => self.on_resize(w, h),
            Event::Input(_) | Event::AgentOutput => {}
            Event::Tick => {
                if self.esc_at.is_some_and(|t| t.elapsed() >= ESC_TIMEOUT) {
                    self.esc_at = None;
                    self.write_focused(&[0x1b]);
                }
            }
            Event::AgentExited(id) => {
                if let Some(a) = self.agents.get_mut(&id) {
                    a.alive = false;
                    self.status = format!("agent {id} exited");
                    if matches!(self.view, View::Progress(ref v) if *v == id)
                        && matches!(self.mode, Mode::Insert)
                    {
                        self.mode = Mode::Normal;
                    }
                }
            }
            Event::Snapshot(Ok(s)) => {
                if s.taken < self.last_mutation {
                    let _ = self.refresh.send(());
                    return;
                }
                self.all = s.all;
                self.ready = s.ready;
                self.clamp();
                self.reconcile();
                self.schedule();
            }
            Event::Snapshot(Err(e)) => self.status = format!("bd: {e}"),
        }
    }

    pub fn on_resize(&mut self, w: u16, h: u16) {
        self.term = (w, h);
        let (r, c) = self.pty_size();
        for a in self.agents.values_mut() {
            a.resize(r, c);
        }
    }

    fn clamp(&mut self) {
        let lens = self.columns().map(|c| c.len());
        for (row, len) in self.rows.iter_mut().zip(lens) {
            *row = (*row).min(len.saturating_sub(1));
        }
    }

    fn reconcile(&mut self) {
        let known: HashSet<String> = self.agents.keys().cloned().collect();
        let orphans: Vec<Bead> = scheduler::orphans(&self.all, &known)
            .into_iter()
            .filter(|b| !self.failed.contains(&b.id))
            .cloned()
            .collect();
        for b in orphans {
            if self.git.worktree_path(&b.id).join(".git").exists() {
                tracing::info!("resuming orphaned agent {}", b.id);
                let r = self.start_agent(&b, true, None);
                if self.report(r).is_none() {
                    self.failed.insert(b.id.clone());
                }
            } else {
                tracing::info!("resetting orphaned bead {}", b.id);
                let r = self
                    .beads
                    .clear_state(&b)
                    .and_then(|_| self.beads.unclaim(&b.id));
                self.report(r);
                self.mutated();
            }
        }
    }

    fn schedule(&mut self) {
        let free = self.cfg.max_agents.saturating_sub(self.running_count());
        if free == 0 {
            return;
        }
        let busy: HashSet<String> = self.agents.keys().cloned().collect();
        let picks: Vec<Bead> = scheduler::pick(&self.ready, &busy, &self.failed, free)
            .into_iter()
            .cloned()
            .collect();
        for b in picks {
            tracing::info!("scheduling {}", b.id);
            let r = self
                .beads
                .claim(&b.id)
                .and_then(|_| self.beads.set_state(&b.id, State::Running))
                .and_then(|_| self.start_agent(&b, false, None));
            if self.report(r).is_none() {
                self.failed.insert(b.id.clone());
                let _ = self.beads.unclaim(&b.id);
            } else {
                self.status = format!("started {}", b.id);
            }
            self.mutated();
        }
    }

    fn start_agent(&mut self, bead: &Bead, resume: bool, prompt: Option<String>) -> Result<()> {
        let worktree = self.git.ensure_worktree(&bead.id, &self.base)?;
        let bead = self.beads.show(&bead.id).unwrap_or_else(|_| bead.clone());
        let launch = Launch {
            bead,
            worktree,
            resume,
            prompt,
        };
        let (r, c) = self.pty_size();
        let agent = Agent::spawn(
            self.backend.as_ref(),
            &launch,
            self.cfg.kache,
            r,
            c,
            self.tx.clone(),
        )?;
        self.agents.insert(launch.bead.id, agent);
        Ok(())
    }

    fn write_focused(&mut self, bytes: &[u8]) {
        if let View::Progress(id) = &self.view
            && let Some(a) = self.agents.get_mut(id)
        {
            a.write(bytes);
        }
    }

    fn focused_app_cursor(&self) -> (bool, bool) {
        match &self.view {
            View::Progress(id) => self
                .agents
                .get(id)
                .map(|a| {
                    let p = a.parser.lock().unwrap();
                    (
                        p.screen().application_cursor(),
                        p.screen().bracketed_paste(),
                    )
                })
                .unwrap_or_default(),
            _ => (false, false),
        }
    }

    fn on_paste(&mut self, text: &str) {
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

    fn on_key(&mut self, k: KeyEvent) {
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
            Mode::Help => {}
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
                    self.create_bead(&title, form.description.trim(), form.priority, form.backlog);
                    return;
                }
            }
            KeyCode::Enter => form.field = 2,
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % 4,
            KeyCode::BackTab | KeyCode::Up => form.field = (form.field + 3) % 4,
            KeyCode::Right | KeyCode::Char('-') if form.field == 2 => {
                form.priority = (form.priority + 1).min(4)
            }
            KeyCode::Left | KeyCode::Char('+') | KeyCode::Char('=') if form.field == 2 => {
                form.priority = form.priority.saturating_sub(1)
            }
            KeyCode::Char(c @ '0'..='4') if form.field == 2 => form.priority = c as u8 - b'0',
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.field == 3 => {
                form.backlog = !form.backlog
            }
            KeyCode::Char('b') if form.field == 3 => form.backlog = true,
            KeyCode::Char('p') if form.field == 3 => form.backlog = false,
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

    fn submit(&mut self, kind: InputKind, text: String) {
        if text.is_empty() {
            return;
        }
        match kind {
            InputKind::Feedback(id) => self.feedback(&id, &text),
            InputKind::Command => self.command(&text),
            InputKind::Comment => {
                if let View::Review(d) = &mut self.view {
                    if d.add_comment(&text) {
                        self.status = format!("{} comment(s), S to send", d.comments.len());
                    } else {
                        self.status = "cannot comment on this line".into();
                    }
                }
            }
        }
    }

    fn command(&mut self, cmd: &str) {
        let (name, rest) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let rest = rest.trim();
        match name {
            "q" | "quit" => self.request_quit(),
            "q!" | "quit!" => self.quit = true,
            "new" | "n" if !rest.is_empty() => self.create_bead(rest, "", 2, false),
            "max" => match rest.parse() {
                Ok(n) => {
                    self.cfg.max_agents = n;
                    self.status = format!("max agents = {n}");
                    self.schedule();
                }
                Err(_) => self.status = format!("max agents = {}", self.cfg.max_agents),
            },
            "refresh" | "r" => {
                self.failed.clear();
                self.mutated();
            }
            "kill" if !rest.is_empty() => self.confirm_kill(rest.to_string()),
            _ => self.status = format!("unknown command: {cmd}"),
        }
    }

    fn create_bead(&mut self, title: &str, description: &str, priority: u8, backlog: bool) {
        let r = self.beads.create(title, description, priority, backlog);
        if let Some(id) = self.report(r) {
            self.status = format!("created {id}");
            self.mutated();
        }
    }

    fn request_quit(&mut self) {
        let live = self.agents.values().filter(|a| a.alive).count();
        if live == 0 {
            self.quit = true;
        } else {
            self.mode = Mode::Confirm {
                action: Action::Quit,
                msg: format!(
                    "{live} live agent(s) will be stopped and resumed next launch. Quit? y/n"
                ),
            };
        }
    }

    fn confirm_kill(&mut self, id: String) {
        self.mode = Mode::Confirm {
            msg: format!("Kill agent {id} and hold the bead? y/n"),
            action: Action::Kill(id),
        };
    }

    fn confirm(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit = true,
            Action::Kill(id) => self.kill(&id),
            Action::Merge(id) => self.merge(&id),
            Action::ResolveConflicts(id) => self.resolve_conflicts(&id),
            Action::Discard(id) => self.discard(&id),
        }
    }

    fn kill(&mut self, id: &str) {
        if let Some(mut a) = self.agents.remove(id) {
            a.kill();
        }
        let r = self
            .beads
            .set_state(id, State::Held)
            .and_then(|_| self.beads.unclaim(id));
        if self.report(r).is_some() {
            self.status = format!("killed {id}, held in Pending (s to release)");
        }
        if matches!(self.view, View::Progress(_)) {
            self.view = View::Board;
        }
        self.mutated();
    }

    fn release(&mut self, bead: &Bead) {
        self.failed.remove(&bead.id);
        if matches!(bead.state(), Some(State::Held | State::Backlog)) {
            let r = self.beads.clear_state(bead);
            if self.report(r).is_some() {
                self.status = format!("released {}", bead.id);
            }
            self.mutated();
        }
    }

    fn move_to_backlog(&mut self, bead: &Bead) {
        if self.agents.contains_key(&bead.id) || bead.status == "closed" {
            return;
        }
        let r = self.beads.set_state(&bead.id, State::Backlog);
        if self.report(r).is_some() {
            self.status = format!("{} moved to backlog (s to release)", bead.id);
        }
        self.mutated();
    }

    fn mark_review(&mut self, id: &str) {
        let r = self.beads.set_state(id, State::Review);
        if self.report(r).is_some() {
            self.status = format!("{id} ready for review");
        }
        self.mutated();
    }

    fn confirm_discard(&mut self, id: String) {
        self.mode = Mode::Confirm {
            msg: format!("Discard {id}: kill agent, delete worktree and branch, hold bead? y/n"),
            action: Action::Discard(id),
        };
    }

    fn discard(&mut self, id: &str) {
        if let Some(mut a) = self.agents.remove(id) {
            a.kill();
        }
        let r = self
            .git
            .remove(id)
            .and_then(|_| self.beads.set_state(id, State::Held))
            .and_then(|_| self.beads.unclaim(id));
        if self.report(r).is_some() {
            self.status = format!("discarded {id}, held in Pending (s to release)");
        }
        self.view = View::Board;
        self.mutated();
    }

    fn merge(&mut self, id: &str) {
        let title = self.bead(id).map(|b| b.title.clone()).unwrap_or_default();
        if let Some(mut a) = self.agents.remove(id) {
            a.kill();
        }
        let msg = format!("{id}: {title}");
        let r = self
            .git
            .merge(id, &self.base, &msg)
            .and_then(|_| self.beads.close(id, "merged by sulla"));
        if let Err(e) = &r {
            if e.downcast_ref::<RebaseConflict>().is_some() {
                self.mode = Mode::Confirm {
                    msg: format!(
                        "Merge of {id} hit conflicts with {}. Have the agent resolve them automatically? y/n",
                        self.base
                    ),
                    action: Action::ResolveConflicts(id.to_string()),
                };
                self.status = format!("merge conflicts in {id}");
                self.mutated();
                return;
            }
        }
        if self.report(r).is_some() {
            self.status = format!("merged {id} into {}", self.base);
            self.view = View::Board;
        }
        self.mutated();
    }

    fn resolve_conflicts(&mut self, id: &str) {
        let text = format!(
            "Merging into {base} failed with conflicts. Run `git rebase {base}`, resolve every conflict while preserving the intent of both sides, finish the rebase, and make sure the build and tests still pass. Do not push or merge. When done, run `bd set-state {id} sulla=review --reason \"ready for review\"` then stop and wait.",
            base = self.base,
            id = id
        );
        self.feedback(id, &text);
    }

    fn feedback(&mut self, id: &str, text: &str) {
        let r = self.beads.set_state(id, State::Running);
        self.report(r);
        match self.agents.get_mut(id) {
            Some(a) if a.alive => {
                a.submit(text);
            }
            _ => {
                self.agents.remove(id);
                if let Some(b) = self.bead(id).cloned() {
                    let r = self.start_agent(&b, true, Some(text.to_string()));
                    self.report(r);
                }
            }
        }
        self.status = format!("feedback sent to {id}");
        self.parked_review = None;
        self.view = View::Progress(id.to_string());
        self.mutated();
    }

    fn relaunch(&mut self, id: &str) {
        if self.agents.get(id).is_some_and(|a| a.alive) {
            return;
        }
        self.agents.remove(id);
        let Some(b) = self.bead(id).cloned() else {
            return;
        };
        let resume = self.git.worktree_path(id).join(".git").exists();
        let r = self.start_agent(&b, resume, None);
        if self.report(r).is_some() {
            self.failed.remove(id);
            if b.state() != Some(State::Review) {
                let r = self.beads.set_state(id, State::Running);
                self.report(r);
            }
            self.status = format!("launched {id}");
            self.mutated();
        }
    }

    fn open_progress(&mut self, id: String) {
        if let Some(a) = self.agents.get_mut(&id) {
            a.scroll_reset();
        }
        self.view = View::Progress(id);
    }

    fn swap_to_agent(&mut self, insert: bool) {
        let view = std::mem::replace(&mut self.view, View::Board);
        let View::Review(d) = view else {
            self.view = view;
            return;
        };
        let id = d.id.clone();
        self.parked_review = Some(d);
        self.open_progress(id.clone());
        if insert {
            self.enter_insert(&id);
        }
    }

    fn back_to_review(&mut self, id: String) -> bool {
        match self.parked_review.take() {
            Some(d) if d.id == id => {
                self.view = View::Review(d);
                self.status = format!("back to review of {id} (R reloads diff)");
                true
            }
            _ => false,
        }
    }

    fn enter_insert(&mut self, id: &str) {
        if !self.agents.get(id).is_some_and(|a| a.alive) {
            self.relaunch(id);
        }
        if let Some(a) = self.agents.get_mut(id)
            && a.alive
        {
            a.scroll_reset();
            self.mode = Mode::Insert;
        }
    }

    fn open_review(&mut self, id: String) {
        let done =
            self.bead(&id).and_then(|b| b.column(Utc::now(), i64::MAX)) == Some(Column::Done);
        let r = if done {
            self.git.merged_diff(&id)
        } else {
            self.git.diff(&id, &self.base)
        };
        if let Some(text) = self.report(r) {
            self.view = View::Review(Diff::parse(&id, &text));
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        let pending = self.pending.take();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('?') => return self.mode = Mode::Help,
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
            View::Review(_) => self.review_key(k, pending),
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
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('n') => {
                self.mode = Mode::NewBead(NewBeadForm::default());
            }
            _ => self.board_bead_key(k),
        }
    }

    fn inspect(&mut self, id: String) {
        let r = self.beads.show_text(&id);
        if let Some(text) = self.report(r) {
            self.mode = Mode::Inspect {
                id,
                text,
                scroll: 0,
            };
        }
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
            KeyCode::Char('p') => return self.open_progress(id),
            KeyCode::Char('r') => return self.open_review(id),
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
                Column::Review | Column::Done => self.open_review(b.id),
                _ => self.open_progress(b.id),
            },
            KeyCode::Char('i') => self.inspect(b.id),
            KeyCode::Char('p') => self.open_progress(b.id),
            KeyCode::Char('r') => self.open_review(b.id),
            KeyCode::Char('X') => self.confirm_kill(b.id),
            KeyCode::Char('D') if Column::ALL[self.col] != Column::Done => {
                self.confirm_discard(b.id)
            }
            KeyCode::Char('R') => self.mark_review(&b.id),
            KeyCode::Char('s') => self.release(&b),
            KeyCode::Char('B') => self.move_to_backlog(&b),
            KeyCode::Char('A') => self.relaunch(&b.id),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let r = self.beads.set_priority(&b.id, b.priority.saturating_sub(1));
                self.report(r);
                self.mutated();
            }
            KeyCode::Char('-') => {
                let r = self.beads.set_priority(&b.id, (b.priority + 1).min(4));
                self.report(r);
                self.mutated();
            }
            _ => {}
        }
    }

    fn progress_key(&mut self, k: KeyEvent, pending: Option<char>) {
        let Some(id) = self.focused_id() else { return };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.pty_size().0 / 2) as isize;
        let agent = self.agents.get_mut(&id);
        match (k.code, agent) {
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
            (KeyCode::Char('G'), Some(a)) => a.scroll_reset(),
            (KeyCode::Char('X'), _) => self.confirm_kill(id),
            (KeyCode::Char('R'), _) => self.mark_review(&id),
            (KeyCode::Char('r'), _) => {
                if !self.back_to_review(id.clone()) {
                    self.open_review(id);
                }
            }
            (KeyCode::Char('A'), _) => self.relaunch(&id),
            _ => {}
        }
    }

    fn review_key(&mut self, k: KeyEvent, pending: Option<char>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let half = (self.pty_size().0 / 2) as isize;
        let View::Review(d) = &mut self.view else {
            return;
        };
        let id = d.id.clone();
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => self.view = View::Board,
            KeyCode::Char('j') | KeyCode::Down => d.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => d.move_cursor(-1),
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
                self.mode = Mode::Input {
                    kind: InputKind::Comment,
                    buf: String::new(),
                };
            }
            KeyCode::Char('x') => {
                let n = d.delete_comments_at_cursor();
                self.status = format!("deleted {n} comment(s)");
            }
            KeyCode::Char('S') => match d.feedback() {
                Some(text) => self.feedback(&id, &text),
                None => self.status = "no comments to send".into(),
            },
            KeyCode::Char('m') => {
                self.mode = Mode::Confirm {
                    msg: format!("Rebase {id} onto {} and fast-forward merge? y/n", self.base),
                    action: Action::Merge(id),
                };
            }
            KeyCode::Char('s') => {
                self.mode = Mode::Input {
                    kind: InputKind::Feedback(id),
                    buf: String::new(),
                };
            }
            KeyCode::Char('D') => self.confirm_discard(id),
            KeyCode::Char('p') | KeyCode::Char('a') => self.swap_to_agent(false),
            KeyCode::Char('i') => self.swap_to_agent(true),
            KeyCode::Char('R') => self.open_review(id),
            _ => {}
        }
    }

    pub fn shutdown(&mut self) {
        for a in self.agents.values_mut() {
            a.kill();
        }
    }
}
