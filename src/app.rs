use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use chrono::Utc;
use color_eyre::{
    Result,
    eyre::{bail, eyre},
};
use crossterm::event::{
    Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};

use crate::agent::{self, Agent, Backend, CHAT_ID, Claude, Copilot, Launch, Stall};
use crate::beads::{Bead, Beads, Column, State};
use crate::budget::{self, Budget};
use crate::checks::{self, Check};
use crate::config::{AgentKind, Config, SandboxKind, Trigger, Workflow};
use crate::conflicts::Conflict;
use crate::filter::Filter;
use crate::git::{self, DiffStat, Git, RebaseConflict};
use crate::health::{self, Probe, Tool};
use crate::notify::Notifier;
use crate::pr::{self, PrState, PrStatus};
use crate::presets;
use crate::sandbox::{self, Outcome, Sandbox, Slots};
use crate::scheduler::{self, Backoff, Idle, Snapshot, fmt_duration};
use crate::timeline;
use crate::ui::keys;
use crate::ui::review::{self, AiReview, Comment, Diff, Verdict};
use crate::usage::{self, Usage, Watch};

pub enum Event {
    Input(CEvent),
    Tick,
    AgentOutput,
    AgentExited(String, u64),
    AgentBell(String),
    Snapshot(Result<Snapshot, String>),
    CheckDone(checks::Done),
    Usage(usage::Report),
    Health(Vec<Tool>),
    Pr(String, u64, Result<PrStatus, String>),
}

pub enum View {
    Board,
    Progress(String),
    Review(Diff),
}

pub enum InputKind {
    Command,
    Search(String),
    Feedback(String),
    Retry(String),
    Comment,
    EditComment(usize),
}

pub struct NewBeadForm {
    pub title: String,
    pub description: String,
    pub priority: u8,
    pub agent: Option<AgentKind>,
    pub presets: Vec<String>,
    pub preset: Option<usize>,
    pub backlog: bool,
    pub field: usize,
}

impl NewBeadForm {
    pub const FIELDS: usize = 6;

    pub fn new(presets: Vec<String>) -> Self {
        Self {
            title: String::new(),
            description: String::new(),
            priority: 2,
            agent: None,
            presets,
            preset: None,
            backlog: false,
            field: 0,
        }
    }

    fn cycle_agent(&mut self, forward: bool) {
        let opts: Vec<Option<AgentKind>> = std::iter::once(None)
            .chain(AgentKind::ALL.into_iter().map(Some))
            .collect();
        let i = opts.iter().position(|o| *o == self.agent).unwrap_or(0);
        let n = opts.len();
        self.agent = opts[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }];
    }

    fn cycle_preset(&mut self, forward: bool) {
        let n = self.presets.len() + 1;
        let i = self.preset.map_or(0, |p| p + 1);
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.preset = j.checked_sub(1);
    }

    pub fn labels(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(a) = self.agent {
            out.push(format!("agent:{}", a.name()));
        }
        if let Some(p) = self.preset.and_then(|i| self.presets.get(i)) {
            out.push(format!("{}:{p}", presets::LABEL));
        }
        out
    }
}

pub enum Action {
    Kill(String),
    Merge(String),
    Publish(String),
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
    Health,
    Inspect {
        id: String,
        text: String,
        scroll: u16,
    },
}

const ESC_TIMEOUT: Duration = Duration::from_millis(300);
const STALL_POLL: Duration = Duration::from_secs(1);
const RATE_SCAN_EVERY: Duration = Duration::from_secs(1);

#[derive(Default)]
struct Restacked {
    moved: Vec<String>,
    deferred: Vec<String>,
    stuck: Vec<String>,
}

impl Restacked {
    fn summary(&self) -> String {
        let mut s = String::new();
        if !self.moved.is_empty() {
            s.push_str(&format!(", restacked {}", self.moved.join(", ")));
        }
        if !self.deferred.is_empty() {
            s.push_str(&format!(
                ", {} busy, restacking when idle",
                self.deferred.join(", ")
            ));
        }
        if !self.stuck.is_empty() {
            s.push_str(&format!(
                ", could not restack {} (rebased at merge)",
                self.stuck.join(", ")
            ));
        }
        s
    }
}

pub struct App {
    pub cfg: Config,
    pub base: String,
    repo: PathBuf,
    chat_session: Option<String>,
    chat_sessions: Vec<String>,
    beads: Beads,
    git: Git,
    backends: HashMap<AgentKind, Box<dyn Backend>>,
    sandbox: Box<dyn Sandbox>,
    slots: Slots,
    deferred: HashMap<usize, Outcome>,
    pub all: Vec<Bead>,
    ready: Vec<Bead>,
    pub blocked: HashMap<String, Vec<String>>,
    pub diffstats: HashMap<String, DiffStat>,
    pub conflicts: HashMap<String, Conflict>,
    pub agents: HashMap<String, Agent>,
    failed: HashSet<String>,
    pub backoff: Backoff,
    rate_seen: HashMap<String, (String, usize)>,
    rate_scanned: Instant,
    restack_stuck: HashMap<String, Option<String>>,
    notifier: Notifier,
    pub pr_watch: pr::Watch,
    pr_refresh: Option<Sender<()>>,
    pub stalls: HashMap<String, Stall>,
    stall_checked: Option<Instant>,
    budget_runs: HashMap<String, budget::Run>,
    budget_checked: Option<Instant>,
    pub over_budget: HashMap<String, String>,
    states: Option<HashMap<String, Option<State>>>,
    pub view: View,
    pub parked_review: Option<Diff>,
    pub ai_reviews: HashMap<String, AiReview>,
    pub checks: HashMap<String, Check>,
    check_runs: u64,
    pub usage: HashMap<String, Usage>,
    usage_seen: bool,
    pub run_usage: Usage,
    usage_watch: Sender<Watch>,
    pub mode: Mode,
    pub col: usize,
    pub zoom: bool,
    pub filter: Filter,
    pub rows: [usize; Column::ALL.len()],
    pub status: String,
    pending: Option<char>,
    esc_at: Option<Instant>,
    last_mutation: Instant,
    tx: Sender<Event>,
    refresh: Sender<()>,
    term: (u16, u16),
    pub pending_edit: Option<PathBuf>,
    pub paused: bool,
    pub health: Option<Vec<Tool>>,
    pub bd_error: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(cfg: Config, repo: PathBuf, tx: Sender<Event>) -> Result<Self> {
        let sandbox = sandbox::build(&cfg, &repo)?;
        let git = Git::new(repo.clone());
        git.ensure_ignored()?;
        let base = match &cfg.base_branch {
            Some(b) => b.clone(),
            None => git.current_branch()?,
        };
        let beads = Beads::new(&repo);
        let refresh = scheduler::spawn_poller(
            beads.clone(),
            git.clone(),
            base.clone(),
            Duration::from_secs(cfg.poll_secs.max(1)),
            tx.clone(),
        );
        let pr_refresh = (cfg.workflow == Workflow::Pr).then(|| {
            scheduler::spawn_pr_watcher(
                beads.clone(),
                repo.clone(),
                Duration::from_secs(cfg.pr.poll_secs.max(1)),
                tx.clone(),
            )
        });
        let pr_watch = pr::Watch::load(repo.join(".sulla").join("pr-watch.json"));
        let usage_watch = usage::spawn(Duration::from_secs(cfg.poll_secs.max(1)), tx.clone());
        let mut backends: HashMap<AgentKind, Box<dyn Backend>> = HashMap::new();
        backends.insert(
            AgentKind::Copilot,
            Box::new(Copilot {
                cfg: cfg.copilot.clone(),
            }),
        );
        backends.insert(
            AgentKind::Claude,
            Box::new(Claude {
                cfg: cfg.claude.clone(),
            }),
        );
        Ok(Self {
            notifier: Notifier::new(cfg.notify.clone()),
            pr_watch,
            pr_refresh,
            stalls: HashMap::new(),
            stall_checked: None,
            budget_runs: HashMap::new(),
            budget_checked: None,
            over_budget: HashMap::new(),
            states: None,
            cfg,
            base,
            repo,
            chat_session: None,
            chat_sessions: Vec::new(),
            beads,
            git,
            backends,
            sandbox,
            slots: Slots::default(),
            deferred: HashMap::new(),
            all: Vec::new(),
            ready: Vec::new(),
            blocked: HashMap::new(),
            diffstats: HashMap::new(),
            conflicts: HashMap::new(),
            agents: HashMap::new(),
            failed: HashSet::new(),
            backoff: Backoff::default(),
            rate_seen: HashMap::new(),
            rate_scanned: Instant::now(),
            restack_stuck: HashMap::new(),
            view: View::Board,
            parked_review: None,
            ai_reviews: HashMap::new(),
            checks: HashMap::new(),
            check_runs: 0,
            usage: HashMap::new(),
            usage_seen: false,
            run_usage: Usage::default(),
            usage_watch,
            mode: Mode::Normal,
            col: 0,
            zoom: false,
            filter: Filter::default(),
            rows: [0; Column::ALL.len()],
            status: String::new(),
            pending: None,
            esc_at: None,
            last_mutation: Instant::now(),
            tx,
            refresh,
            term: (80, 24),
            pending_edit: None,
            paused: false,
            health: None,
            bd_error: None,
            quit: false,
        })
    }

    pub fn backend_name(&self) -> &str {
        self.backend(self.cfg.agent).name()
    }

    fn backend(&self, kind: AgentKind) -> &dyn Backend {
        self.backends[&kind].as_ref()
    }

    pub fn sandbox_name(&self) -> &str {
        self.sandbox.name()
    }

    pub fn slot_location(&self, id: &str) -> Option<String> {
        let slot = self.slots.get(id)?;
        let name = self.sandbox.instance(slot)?;
        Some(match self.sandbox.address(slot) {
            Some(ip) => format!("{name} {ip}"),
            None => name,
        })
    }

    pub fn columns(&self) -> [Vec<&Bead>; Column::ALL.len()] {
        let mut cols = self.all_columns();
        if self.filter.is_active() {
            for c in &mut cols {
                c.retain(|b| self.filter.matches(b));
            }
        }
        cols
    }

    pub fn all_columns(&self) -> [Vec<&Bead>; Column::ALL.len()] {
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
            .filter(|(id, a)| a.alive && id.as_str() != CHAT_ID)
            .filter(|(id, _)| {
                agent::reviewed_bead(id).is_some()
                    || self.bead(id).is_none_or(|b| {
                        b.state() != Some(State::Reviewing)
                            && !matches!(
                                b.column(now, i64::MAX),
                                Some(Column::Review | Column::AwaitingPr | Column::Done)
                            )
                    })
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
        self.dispatch(ev);
        self.check_stalls();
        self.check_budgets();
        if let Some(id) = self.viewing() {
            self.notifier.clear(&id);
        }
    }

    fn viewing(&self) -> Option<String> {
        match &self.view {
            View::Board => None,
            View::Progress(id) => Some(agent::reviewed_bead(id).unwrap_or(id).to_string()),
            View::Review(d) => Some(d.id.clone()),
        }
    }

    fn notify(&mut self, trigger: Trigger, id: &str, title: &str) {
        if self.viewing().as_deref() != Some(id) {
            self.notifier.fire(trigger, id, title);
        }
    }

    fn on_bell(&mut self, id: &str) {
        if id == CHAT_ID {
            return self.notify(Trigger::Waiting, CHAT_ID, "conversation");
        }
        let reviewer = agent::reviewed_bead(id);
        let Some(b) = self.bead(reviewer.unwrap_or(id)) else {
            return;
        };
        if reviewer.is_none() && b.state() == Some(State::Review) {
            return;
        }
        let (bid, title) = (b.id.clone(), b.title.clone());
        self.notify(Trigger::Waiting, &bid, &title);
    }

    fn check_stalls(&mut self) {
        let now = Instant::now();
        if self
            .stall_checked
            .is_some_and(|t| now.duration_since(t) < STALL_POLL)
        {
            return;
        }
        self.stall_checked = Some(now);
        let mut next = HashMap::new();
        for (id, a) in &self.agents {
            let (bid, want) = match agent::reviewed_bead(id) {
                Some(b) => (b, State::Reviewing),
                None => (id.as_str(), State::Running),
            };
            let watched = self
                .bead(bid)
                .is_some_and(|b| b.status != "closed" && b.state() == Some(want));
            if let Some(s) = watched.then(|| a.stall(now, &self.cfg.stall)).flatten() {
                next.insert(id.clone(), s);
            }
        }
        for id in &self.failed {
            let orphaned = !self.agents.contains_key(id)
                && self
                    .bead(id)
                    .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Running));
            if orphaned {
                next.insert(id.clone(), Stall::Dead);
            }
        }
        let prev = std::mem::replace(&mut self.stalls, next);
        for id in prev.keys().filter(|id| !self.stalls.contains_key(*id)) {
            let bid = agent::reviewed_bead(id).unwrap_or(id);
            self.notifier
                .retain(|i, t| i != bid || matches!(t, Trigger::Review | Trigger::Budget));
        }
        let fresh: Vec<(String, Stall)> = self
            .stalls
            .iter()
            .filter(|(id, s)| prev.get(*id) != Some(*s))
            .map(|(id, s)| (id.clone(), *s))
            .collect();
        for (id, s) in fresh {
            let bid = agent::reviewed_bead(&id).unwrap_or(&id);
            let Some((bid, title)) = self.bead(bid).map(|b| (b.id.clone(), b.title.clone())) else {
                continue;
            };
            let trigger = match s {
                Stall::Waiting => Trigger::Waiting,
                Stall::Stalled | Stall::Dead => Trigger::Stalled,
            };
            tracing::info!("agent {id} {s:?}");
            self.notify(trigger, &bid, &title);
        }
    }

    pub fn budget_status(&self, id: &str) -> Option<String> {
        let run = self.budget_runs.get(id)?;
        let budget = Budget::for_bead(&self.cfg.budget, self.bead(id)?);
        if budget.is_unlimited() {
            return None;
        }
        let used = run.used(self.usage.get(id).copied().unwrap_or_default());
        Some(budget.progress(run.elapsed(Instant::now()), used))
    }

    fn check_budgets(&mut self) {
        let now = Instant::now();
        if self
            .budget_checked
            .is_some_and(|t| now.duration_since(t) < STALL_POLL)
        {
            return;
        }
        self.budget_checked = Some(now);
        let running: Vec<(String, Budget)> = self
            .all
            .iter()
            .filter(|b| b.status != "closed" && b.state() == Some(State::Running))
            .filter(|b| self.agents.get(&b.id).is_some_and(|a| a.alive))
            .map(|b| (b.id.clone(), Budget::for_bead(&self.cfg.budget, b)))
            .collect();
        self.budget_runs
            .retain(|id, _| running.iter().any(|(r, _)| r == id));
        let mut over = Vec::new();
        for (id, budget) in running {
            let current = self
                .usage_seen
                .then(|| self.usage.get(&id).copied().unwrap_or_default());
            let run = self
                .budget_runs
                .entry(id.clone())
                .or_insert_with(|| budget::Run::new(now, current));
            if let Some(c) = current {
                run.observe(c);
            }
            let used = run.used(current.unwrap_or_default());
            if let Some(msg) = budget.exceeded(run.elapsed(now), used) {
                over.push((id, msg));
            }
        }
        for (id, msg) in over {
            self.hold_over_budget(&id, &msg);
        }
    }

    fn hold_over_budget(&mut self, id: &str, msg: &str) {
        tracing::info!("{id}: {msg}");
        self.stop_check(id);
        self.remove_agent(id, Outcome::Killed);
        self.budget_runs.remove(id);
        let r = self
            .beads
            .set_state_because(id, State::Held, msg)
            .and_then(|_| self.beads.unclaim(id));
        if self.report(r).is_some() {
            self.status = format!("{id} {msg}, held in Pending (s to release)");
        }
        self.over_budget.insert(id.to_string(), msg.to_string());
        if matches!(&self.view, View::Progress(v) if v == id) {
            if matches!(self.mode, Mode::Insert) {
                self.mode = Mode::Normal;
            }
            self.view = View::Board;
        }
        if let Some(title) = self.bead(id).map(|b| b.title.clone()) {
            self.notify(Trigger::Budget, id, &title);
        }
        self.mutated();
    }

    fn entered_review(&mut self) -> Vec<String> {
        let now: HashMap<String, Option<State>> =
            self.all.iter().map(|b| (b.id.clone(), b.state())).collect();
        let Some(prev) = self.states.replace(now) else {
            return Vec::new();
        };
        self.all
            .iter()
            .filter(|b| b.status != "closed" && b.state() == Some(State::Review))
            .filter(|b| prev.get(&b.id).is_some_and(|s| *s != Some(State::Review)))
            .map(|b| b.id.clone())
            .collect()
    }

    fn dispatch(&mut self, ev: Event) {
        match ev {
            Event::Input(CEvent::Key(k)) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Input(CEvent::Paste(s)) => self.on_paste(&s),
            Event::Input(CEvent::Resize(w, h)) => self.on_resize(w, h),
            Event::Input(CEvent::Mouse(m)) => self.on_mouse(m),
            Event::Input(_) | Event::AgentOutput => {}
            Event::Tick => {
                if self.esc_at.is_some_and(|t| t.elapsed() >= ESC_TIMEOUT) {
                    self.esc_at = None;
                    self.write_focused(&[0x1b]);
                }
                self.check_rate_limits();
            }
            Event::AgentBell(id) => self.on_bell(&id),
            Event::AgentExited(id, generation) => {
                if let Some(a) = self.agents.get_mut(&id)
                    && a.generation == generation
                {
                    a.exited();
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
                let finished: Vec<String> = s
                    .all
                    .iter()
                    .filter(|b| {
                        b.status != "closed"
                            && b.state() == Some(State::Review)
                            && self.reviewing(&b.id)
                            && self.agents.contains_key(&agent::review_id(&b.id))
                    })
                    .map(|b| b.id.clone())
                    .collect();
                self.bd_error = None;
                self.all = s.all;
                self.ready = s.ready;
                self.blocked = s.blocked;
                self.diffstats = s.diffstats;
                self.conflicts = s.conflicts;
                let all = &self.all;
                self.ai_reviews.retain(|id, r| {
                    all.iter().any(|b| {
                        b.id == *id
                            && b.status != "closed"
                            && (r.forwarded || b.state() != Some(State::Running))
                    })
                });
                self.notifier.retain(|id, t| {
                    id == CHAT_ID
                        || all.iter().any(|b| {
                            b.id == id
                                && b.status != "closed"
                                && (t != Trigger::Review || b.state() == Some(State::Review))
                        })
                });
                let before = self.pr_watch.beads.len();
                self.pr_watch
                    .beads
                    .retain(|id, _| all.iter().any(|b| b.id == *id && b.status != "closed"));
                if self.pr_watch.beads.len() != before
                    && let Err(e) = self.pr_watch.save()
                {
                    tracing::warn!("pr watch: {e:#}");
                }
                self.over_budget.retain(|id, _| {
                    all.iter().any(|b| {
                        b.id == *id && b.status != "closed" && b.state() == Some(State::Held)
                    })
                });
                let entered = self.entered_review();
                self.clamp();
                self.watch_usage();
                self.reconcile();
                for id in &finished {
                    self.collect_ai_review(id);
                }
                for id in entered {
                    if finished.contains(&id)
                        && self.ai_reviews.get(&id).is_some_and(|r| r.forwarded)
                    {
                        continue;
                    }
                    if let Some(title) = self.bead(&id).map(|b| b.title.clone()) {
                        self.notify(Trigger::Review, &id, &title);
                    }
                }
                self.sync_checks();
                self.schedule();
            }
            Event::Snapshot(Err(e)) => {
                self.status = format!("bd: {e}");
                self.bd_error = Some(e);
            }
            Event::CheckDone(d) => self.check_done(d),
            Event::Usage(r) => {
                self.usage_seen = true;
                self.usage = r.per_key;
                self.run_usage = r.run;
            }
            Event::Health(tools) => self.health = Some(tools),
            Event::Pr(id, number, r) => self.on_pr(&id, number, r),
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

    fn reap_reviewers(&mut self) {
        let stale: Vec<String> = self
            .agents
            .keys()
            .filter(|k| {
                agent::reviewed_bead(k).is_some_and(|id| {
                    !self.bead(id).is_some_and(|b| {
                        b.status != "closed" && b.state() == Some(State::Reviewing)
                    })
                })
            })
            .cloned()
            .collect();
        for rid in stale {
            tracing::info!("stopping reviewer {rid}");
            self.remove_agent(&rid, Outcome::Killed);
            if matches!(&self.view, View::Progress(v) if *v == rid) {
                if matches!(self.mode, Mode::Insert) {
                    self.mode = Mode::Normal;
                }
                self.view = View::Board;
            }
        }
    }

    fn reconcile(&mut self) {
        self.reap_reviewers();
        self.sweep_stacks();
        let known: HashSet<String> = self.agents.keys().cloned().collect();
        let mut orphans: Vec<Bead> = scheduler::orphans(&self.all, &known)
            .into_iter()
            .filter(|b| !self.failed.contains(&b.id))
            .cloned()
            .collect();
        orphans.sort_by_key(|b| b.state() != Some(State::Running));
        for b in orphans {
            let state = b.state();
            if state == Some(State::Reviewing) {
                tracing::info!("returning orphaned review {} to review", b.id);
                let r = self.beads.set_state(&b.id, State::Review);
                self.report(r);
                self.mutated();
                if known.contains(&b.id) {
                    continue;
                }
            }
            let has_worktree = self.git.worktree_path(&b.id).join(".git").exists();
            if matches!(state, Some(State::Review | State::Reviewing))
                && (!has_worktree || self.slots.free(self.cfg.max_agents) == 0)
            {
                continue;
            }
            if has_worktree {
                if self.backoff.remaining(Instant::now()).is_some() {
                    continue;
                }
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

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.status = "scheduling paused".into();
        } else {
            self.backoff.resume();
            self.status = "scheduling resumed".into();
            self.schedule();
        }
    }

    fn check_rate_limits(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.rate_scanned) < RATE_SCAN_EVERY {
            return;
        }
        self.rate_scanned = now;
        let mut seen = HashMap::new();
        let mut fresh = None;
        for (id, a) in &self.agents {
            if !a.alive {
                continue;
            }
            if let Some(hit) = a.rate_limit() {
                if self.rate_seen.get(id) != Some(&hit) && fresh.is_none() {
                    fresh = Some(id.clone());
                }
                seen.insert(id.clone(), hit);
            }
        }
        self.rate_seen = seen;
        if let Some(id) = fresh {
            if let Some(d) = self.backoff.hit(now) {
                tracing::warn!("rate limit from {id}, pausing scheduler for {d:?}");
                self.status = format!(
                    "rate limit from {id}, pausing scheduler for {} (:resume to override)",
                    fmt_duration(d)
                );
            }
        } else if self.backoff.expired(now) {
            self.status = "rate limit backoff over, scheduler resumed".into();
            self.mutated();
        }
    }

    fn free_slots(&self) -> usize {
        self.cfg
            .max_agents
            .saturating_sub(self.running_count())
            .min(self.slots.free(self.cfg.max_agents))
    }

    fn picks(&self, free: usize) -> Vec<Bead> {
        let busy: HashSet<String> = self.agents.keys().cloned().collect();
        let mut candidates = self.ready.clone();
        candidates.extend(
            scheduler::stackable(&self.all, &self.blocked)
                .into_iter()
                .filter(|b| self.stack_on(&b.id).is_some())
                .cloned(),
        );
        candidates.extend(scheduler::revisable(&self.all).cloned());
        scheduler::pick(&candidates, &busy, &self.failed, free)
            .into_iter()
            .cloned()
            .collect()
    }

    pub fn idle(&self) -> Option<Idle> {
        if self.paused {
            return Some(Idle::Paused);
        }
        if let Some(d) = self.backoff.remaining(Instant::now()) {
            return Some(Idle::RateLimited(d));
        }
        let free = self.free_slots();
        if free == 0 {
            return Some(Idle::SlotsFull {
                running: self.running_count(),
                max: self.cfg.max_agents,
            });
        }
        if !self.picks(free).is_empty() {
            return None;
        }
        Some(scheduler::why_idle(&self.all, &self.blocked, &self.failed))
    }

    fn open_health(&mut self) {
        let used: HashSet<AgentKind> = std::iter::once(self.cfg.agent)
            .chain(
                self.all
                    .iter()
                    .filter(|b| b.status != "closed")
                    .map(|b| agent::selection(b, self.cfg.agent).0),
            )
            .collect();
        let mut probes = vec![Probe::new("bd", "bd")];
        for kind in AgentKind::ALL.into_iter().filter(|k| used.contains(k)) {
            let bin = match kind {
                AgentKind::Copilot => &self.cfg.copilot.bin,
                AgentKind::Claude => &self.cfg.claude.bin,
            };
            probes.push(Probe::new(self.backend(kind).name(), bin));
        }
        probes.extend([
            Probe::new("kache", "kache")
                .off((!self.cfg.kache).then_some("disabled (kache = false)")),
            Probe::new("incus", &self.cfg.incus.bin)
                .off(
                    (self.cfg.sandbox != SandboxKind::Incus)
                        .then_some("not used (sandbox = worktree)"),
                )
                .also(&["query", "/1.0"]),
        ]);
        if self.cfg.workflow == Workflow::Pr {
            probes.push(pr::probe());
        }
        health::spawn(probes, self.tx.clone());
        self.mode = Mode::Health;
    }

    fn schedule(&mut self) {
        if self.paused || self.backoff.remaining(Instant::now()).is_some() {
            return;
        }
        let free = self.free_slots();
        if free == 0 {
            return;
        }
        let picks = self.picks(free);
        for b in picks {
            if b.state() == Some(State::Revise) {
                self.revise(&b);
                continue;
            }
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

    fn revise(&mut self, bead: &Bead) {
        let id = bead.id.clone();
        tracing::info!("revising {id} from PR feedback");
        let r = self.start_revision(bead);
        if self.report(r).is_some() {
            self.failed.remove(&id);
            self.status = format!("revising {id} from PR feedback");
        } else {
            self.failed.insert(id.clone());
            let r = self.beads.set_state(&id, State::Revise);
            self.report(r);
        }
        self.mutated();
    }

    fn start_revision(&mut self, bead: &Bead) -> Result<()> {
        let id = bead.id.as_str();
        let number: u64 = bead
            .label("pr")
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| eyre!("{id} has no pr:<number> label"))?;
        if !self.git.worktree_path(id).join(".git").exists() {
            return Err(eyre!("{id} has no worktree to revise"));
        }
        let conflict = self.rebase_for_revision(id);
        let pending = self
            .pr_watch
            .beads
            .get(id)
            .map(|s| s.pending.clone())
            .unwrap_or_default();
        let failures = pr::failures(&self.repo, number, &pending.failing);
        let text = pr::revise_prompt(
            id,
            number,
            &pending.comments,
            &failures,
            conflict.as_deref(),
        );
        self.git.mark_revision(id)?;
        self.budget_runs.remove(id);
        self.beads
            .set_state_because(id, State::Running, "revising PR feedback")?;
        match self.agents.get_mut(id) {
            Some(a) if a.alive => a.submit(&text),
            _ => {
                self.remove_agent(id, Outcome::Exited);
                self.start_agent(bead, true, Some(text))?;
            }
        }
        self.pr_watch
            .beads
            .entry(id.to_string())
            .or_default()
            .take();
        if let Err(e) = self.pr_watch.save() {
            tracing::warn!("pr watch: {e:#}");
        }
        Ok(())
    }

    fn rebase_for_revision(&self, id: &str) -> Option<String> {
        let onto = self
            .git
            .fetch_base(&self.cfg.pr.remote, &self.base)
            .map(|r| r.trim_start_matches("refs/remotes/").to_string())
            .unwrap_or_else(|e| {
                tracing::warn!("fetch {}: {e:#}", self.base);
                self.base.clone()
            });
        let up = self.git.upstream(id, &onto);
        let e = self.git.rebase(id, &onto).err()?;
        let Some(c) = e.downcast_ref::<RebaseConflict>() else {
            tracing::warn!("rebase of {id} before revision: {e:#}");
            return None;
        };
        let rebase = if up == c.base {
            format!("git rebase {}", c.base)
        } else {
            format!("git rebase --onto {} {up}", c.base)
        };
        Some(format!(
            "Rebasing onto the latest {} hit conflicts. First run `{rebase}`, resolve every conflict while preserving the intent of both sides, and finish the rebase.",
            c.base
        ))
    }

    fn request_revise(&mut self, bead: &Bead) {
        if bead.state() != Some(State::Pr) {
            self.status = format!("{} is not awaiting a PR", bead.id);
            return;
        }
        let n = self
            .pr_watch
            .beads
            .get(&bead.id)
            .map_or(0, |s| s.pending.count());
        if n == 0 {
            self.status = format!("no unaddressed PR feedback for {}", bead.id);
            return;
        }
        let r = self
            .beads
            .set_state_because(&bead.id, State::Revise, "PR feedback");
        if self.report(r).is_some() {
            self.status = format!("{} queued to revise {n} PR feedback item(s)", bead.id);
        }
        self.mutated();
    }

    fn stack_on(&self, id: &str) -> Option<String> {
        scheduler::stack_parent(id, &self.blocked, &self.all)
            .filter(|p| self.git.has_branch(p))
            .map(String::from)
    }

    fn start_agent(&mut self, bead: &Bead, resume: bool, prompt: Option<String>) -> Result<()> {
        let fresh = !self.git.worktree_path(&bead.id).join(".git").exists()
            && !self.git.has_branch(&bead.id);
        let worktree = match self.stack_on(&bead.id).filter(|_| fresh) {
            Some(parent) => self.git.ensure_stacked_worktree(&bead.id, &parent)?,
            None => self.git.ensure_worktree(&bead.id, &self.base)?,
        };
        let parent = self.git.stack_parent(&bead.id);
        let bead = self.beads.show(&bead.id).unwrap_or_else(|_| bead.clone());
        let prompt = prompt.or_else(|| {
            (!resume).then(|| {
                let preset = presets::for_bead(&self.cfg.presets, &bead);
                agent::task_prompt(&bead, parent.as_deref(), preset)
            })
        });
        let (kind, model) = agent::selection(&bead, self.cfg.agent);
        let slot = self
            .slots
            .acquire_with(&bead.id, &agent::review_id(&bead.id), self.cfg.max_agents)
            .ok_or_else(|| eyre!("no free agent slot for {}", bead.id))?;
        let launch = Launch {
            session: agent::session_id(&self.repo, &bead.id),
            id: bead.id.clone(),
            bead: true,
            worktree,
            resume,
            prompt,
            model,
        };
        let r = self.spawn_sandboxed(slot, launch, kind);
        if let Err(e) = r {
            self.slots.release(&bead.id);
            return Err(e);
        }
        Ok(())
    }

    fn start_review(&mut self, id: &str) {
        let Some(b) = self.bead(id).cloned() else {
            return;
        };
        if self
            .agents
            .get(&agent::review_id(id))
            .is_some_and(|a| a.alive)
        {
            self.status = format!("reviewer for {id} is already running");
            return;
        }
        if b.status == "closed" || b.state() != Some(State::Review) {
            self.status = format!("{id} is not in review");
            return;
        }
        if self.slots.get(id).is_none() && self.running_count() >= self.cfg.max_agents {
            self.status = format!("all {} agent slots are busy", self.cfg.max_agents);
            return;
        }
        let r = self.launch_review(&b);
        if self.report(r).is_some() {
            self.status = format!("ai review of {id} started (p to watch)");
        }
        self.mutated();
    }

    fn launch_review(&mut self, bead: &Bead) -> Result<()> {
        let worktree = self.git.worktree_path(&bead.id);
        if !worktree.join(".git").exists() {
            bail!("no worktree for {}", bead.id);
        }
        let rid = agent::review_id(&bead.id);
        self.remove_agent(&rid, Outcome::Killed);
        self.ai_reviews.remove(&bead.id);
        self.inject_ai(&bead.id);
        let bead = self.beads.show(&bead.id).unwrap_or_else(|_| bead.clone());
        let slot = self
            .slots
            .acquire_with(&rid, &bead.id, self.cfg.max_agents)
            .ok_or_else(|| eyre!("no free agent slot for {rid}"))?;
        let (kind, model) = agent::selection(&bead, self.cfg.agent);
        let launch = Launch {
            session: agent::session_id(&self.repo, &rid),
            id: rid.clone(),
            bead: true,
            worktree,
            resume: false,
            prompt: Some(agent::review_prompt(
                &bead,
                &self.git.upstream(&bead.id, &self.base),
            )),
            model,
        };
        let r = self
            .beads
            .set_state(&bead.id, State::Reviewing)
            .and_then(|_| self.spawn_sandboxed(slot, launch, kind));
        if let Err(e) = r {
            self.slots.release(&rid);
            let _ = self.beads.set_state(&bead.id, State::Review);
            return Err(e);
        }
        Ok(())
    }

    fn collect_ai_review(&mut self, id: &str) {
        let text = match self.beads.latest_comment(id) {
            Ok(Some(t)) => t,
            Ok(None) => {
                self.status = format!("ai review of {id}: no comment found, left in review");
                return;
            }
            Err(e) => {
                tracing::warn!("{e:#}");
                self.status = format!("ai review of {id}: {e}");
                return;
            }
        };
        let Some(out) = review::parse_ai_review(&text) else {
            self.status = format!("ai review of {id}: unparseable output, left in review");
            return;
        };
        let r = self.load_diff(id);
        let Some(diff) = self.report(r) else {
            return;
        };
        let comments: Vec<Comment> = out.comments.iter().map(|c| diff.anchor(c)).collect();
        let json = match out.verdict {
            Verdict::Changes => review::feedback_json(id, &comments),
            Verdict::Approve => None,
        };
        let n = comments.len();
        self.ai_reviews.insert(
            id.to_string(),
            AiReview {
                verdict: out.verdict,
                comments,
                forwarded: json.is_some(),
            },
        );
        self.inject_ai(id);
        match json {
            Some(json) => {
                self.deliver(id, &json);
                self.status = format!("ai review of {id}: {n} comment(s) sent to agent");
                self.mutated();
            }
            None if out.verdict == Verdict::Approve => {
                self.status = format!("ai review of {id}: approved, {n} comment(s)");
            }
            None => {
                self.status =
                    format!("ai review of {id}: changes without comments, left in review");
            }
        }
    }

    fn inject_ai(&mut self, id: &str) {
        let ai: Vec<Comment> = self
            .ai_reviews
            .get(id)
            .map(|r| r.comments.clone())
            .unwrap_or_default();
        let open = match &mut self.view {
            View::Review(d) => Some(d),
            _ => None,
        };
        for d in [open, self.parked_review.as_mut()]
            .into_iter()
            .flatten()
            .filter(|d| d.id == id)
        {
            d.comments.retain(|c| !c.ai);
            d.comments.extend(ai.iter().cloned());
        }
    }

    fn sync_ai(&mut self) {
        let View::Review(d) = &self.view else {
            return;
        };
        if let Some(r) = self.ai_reviews.get_mut(&d.id) {
            r.comments = d.comments.iter().filter(|c| c.ai).cloned().collect();
        }
    }

    fn check_cmd(&self) -> Option<String> {
        self.cfg.check.clone().filter(|c| !c.trim().is_empty())
    }

    fn in_review(&self, id: &str) -> bool {
        self.bead(id).is_some_and(|b| {
            b.status != "closed" && matches!(b.state(), Some(State::Review | State::Reviewing))
        })
    }

    fn check_key(&self, id: &str) -> Result<checks::Key> {
        Ok(checks::Key {
            head: self.git.head(id)?,
            dirty: self.git.dirty(id)?,
        })
    }

    fn check_log(&self, id: &str) -> PathBuf {
        checks::log_path(&self.repo.join(".sulla").join("checks"), id)
    }

    fn agent_log(&self, id: &str) -> PathBuf {
        agent::log_path(&self.repo.join(".sulla").join("logs"), id)
    }

    fn show_log(&mut self, id: String) {
        let (r, c) = self.pty_size();
        let render = |path: PathBuf| {
            std::fs::read(path)
                .ok()
                .map(|b| agent::render_log(&b, r, c))
        };
        let main = render(self.agent_log(&id));
        let review = render(self.agent_log(&agent::review_id(&id)));
        let text = match (main, review) {
            (None, None) => {
                self.status = format!("no session log for {id}");
                return;
            }
            (Some(m), None) => m,
            (None, Some(v)) => format!("--- ai review ---\n{v}"),
            (Some(m), Some(v)) => format!("{m}\n\n--- ai review ---\n{v}"),
        };
        let scroll = text.lines().count().saturating_sub(1) as u16;
        self.mode = Mode::Inspect { id, text, scroll };
    }

    fn sync_checks(&mut self) {
        let stale: Vec<String> = self
            .checks
            .keys()
            .filter(|id| self.check_cmd().is_none() || !self.in_review(id))
            .cloned()
            .collect();
        for id in stale {
            self.stop_check(&id);
        }
        if self.check_cmd().is_none() {
            return;
        }
        let ids: Vec<String> = self
            .all
            .iter()
            .filter(|b| self.in_review(&b.id))
            .map(|b| b.id.clone())
            .collect();
        for id in ids {
            if !self.git.worktree_path(&id).join(".git").exists() {
                continue;
            }
            let Ok(key) = self.check_key(&id) else {
                continue;
            };
            if self.checks.get(&id).is_some_and(|c| c.key == key) {
                continue;
            }
            self.stop_check(&id);
            if let Some(c) = Check::restore(self.check_log(&id), &key) {
                self.checks.insert(id, c);
            } else if !self.sandbox.isolated() || self.slots.get(&id).is_some() {
                self.enqueue_check(&id, key, false);
            }
        }
        self.pump_checks();
    }

    fn enqueue_check(&mut self, id: &str, key: checks::Key, host: bool) {
        self.stop_check(id);
        self.check_runs += 1;
        let c = Check::queued(key, self.check_log(id), self.check_runs, host);
        self.checks.insert(id.to_string(), c);
    }

    fn pump_checks(&mut self) {
        if self
            .checks
            .values()
            .any(|c| c.status == checks::Status::Running)
        {
            return;
        }
        let Some(id) = self
            .checks
            .iter()
            .filter(|(_, c)| c.status == checks::Status::Queued)
            .min_by_key(|(_, c)| c.queued)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let Some(cmd) = self.check_cmd() else {
            return;
        };
        let dir = self.git.worktree_path(&id);
        let mut env = vec![("SULLA_BEAD".to_string(), id.clone())];
        if self.cfg.kache {
            env.push(("RUSTC_WRAPPER".into(), "kache".into()));
        }
        let argv: Vec<std::ffi::OsString> = ["sh", "-c", &cmd].map(Into::into).into();
        let host = self.checks[&id].host;
        let command = if !self.sandbox.isolated() || host {
            Some(sandbox::host_command(&dir, &argv, &env))
        } else {
            self.slots
                .get(&id)
                .map(|slot| self.sandbox.command(slot, &dir, &argv, &env))
        };
        let tx = self.tx.clone();
        let c = self.checks.get_mut(&id).unwrap();
        let r = match command {
            Some(command) => c.launch(&id, command, tx),
            None => Err(eyre!(
                "no sandbox container for {id}, T runs checks on the host"
            )),
        };
        if let Err(e) = r {
            tracing::warn!("check for {id} did not start: {e:#}");
            c.fail(&e.to_string());
            self.pump_checks();
        }
    }

    fn check_done(&mut self, d: checks::Done) {
        let key = self.check_key(&d.id).ok();
        if let Some(c) = self.checks.get_mut(&d.id)
            && c.run == d.run
            && c.status == checks::Status::Running
        {
            c.finish(d.ok, key);
            self.status = format!("{}: {}", d.id, c.summary());
        }
        self.pump_checks();
    }

    fn stop_check(&mut self, id: &str) {
        if let Some(mut c) = self.checks.remove(id) {
            c.stop();
        }
    }

    fn rerun_check(&mut self, id: &str) {
        if self.check_cmd().is_none() {
            self.status = "no check command configured".into();
            return;
        }
        if !self.in_review(id) {
            self.status = format!("{id} is not in review");
            return;
        }
        let r = self.check_key(id);
        let Some(key) = self.report(r) else {
            return;
        };
        let host = self.sandbox.isolated() && self.slots.get(id).is_none();
        self.enqueue_check(id, key, host);
        self.pump_checks();
        self.status = format!(
            "checks for {id} queued{}",
            if host {
                " on the host, no container"
            } else {
                ""
            }
        );
    }

    fn check_blocker(&self, id: &str) -> Option<String> {
        self.check_cmd()?;
        if !self.git.worktree_path(id).join(".git").exists() {
            return None;
        }
        let key = match self.check_key(id) {
            Ok(k) => k,
            Err(_) => return Some("worktree state is unreadable".into()),
        };
        if !key.dirty.is_empty() {
            return Some("the worktree has uncommitted changes".into());
        }
        let Some(c) = self.checks.get(id) else {
            return Some("checks have not run (T runs them)".into());
        };
        match c.status {
            checks::Status::Queued => Some("checks are queued".into()),
            checks::Status::Running => Some("checks are still running".into()),
            _ if c.key != key => Some("checks are stale".into()),
            checks::Status::Fail => Some("checks failed".into()),
            _ => None,
        }
    }

    fn show_check(&mut self, id: String) {
        let (Some(cmd), Some(c)) = (self.check_cmd(), self.checks.get(&id)) else {
            self.status = format!("no check results for {id}");
            return;
        };
        let text = format!("$ {cmd}\n{}\n\n{}", c.summary(), c.output());
        let scroll = text.lines().count().saturating_sub(1) as u16;
        self.mode = Mode::Inspect { id, text, scroll };
    }

    fn review_of(&self, id: &str) -> Option<&Diff> {
        let open = match &self.view {
            View::Review(d) => Some(d),
            _ => None,
        };
        [open, self.parked_review.as_ref()]
            .into_iter()
            .flatten()
            .find(|d| d.id == id)
    }

    fn request_merge(&mut self, id: String, force: bool) {
        if self.review_of(&id).is_some_and(Diff::all_rejected) {
            self.status = format!("every change in {id} is rejected, D discards it");
            return;
        }
        let msg = match self.check_blocker(&id) {
            Some(why) if !force => {
                self.status = format!("{why} for {id}, t shows output, M force merges");
                return;
            }
            Some(why) => format!(
                "Force merge {id} although {why}? Rebase onto {} and fast-forward merge? y/n",
                self.base
            ),
            None => format!("Rebase {id} onto {} and fast-forward merge? y/n", self.base),
        };
        let n = self.review_of(&id).map_or(0, Diff::rejected_hunks);
        let msg = if n > 0 {
            format!("{n} rejected hunk(s) will be dropped. {msg}")
        } else {
            msg
        };
        self.mode = Mode::Confirm {
            msg,
            action: Action::Merge(id),
        };
    }

    fn pr_base(&self, id: &str) -> String {
        self.git
            .stack_parent(id)
            .filter(|p| {
                self.bead(p)
                    .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Pr))
            })
            .map(|p| git::branch(&p))
            .unwrap_or_else(|| self.base.clone())
    }

    fn request_publish(&mut self, id: String) {
        if self.cfg.workflow != Workflow::Pr {
            self.status = "publishing needs workflow = \"pr\", m merges locally".into();
            return;
        }
        if !self.git.has_branch(&id) {
            self.status = format!("{id} has no branch to publish");
            return;
        }
        if self.review_of(&id).is_some_and(Diff::all_rejected) {
            self.status = format!("every change in {id} is rejected, D discards it");
            return;
        }
        let base = self.pr_base(&id);
        if base == self.base {
            let blockers = self.beads.open_blockers(&id);
            let Some(blockers) = self.report(blockers) else {
                return;
            };
            if !blockers.is_empty() {
                self.status = format!(
                    "cannot publish {id}: blocked by open {}",
                    blockers.join(", ")
                );
                return;
            }
        }
        let remote = &self.cfg.pr.remote;
        let br = git::branch(&id);
        let msg = match self.bead(&id).and_then(|b| b.label("pr")) {
            Some(n) => format!("Force push {br} to {remote} and update PR #{n}? y/n"),
            None => format!("Push {br} to {remote} and open a PR against {base}? y/n"),
        };
        let n = self.review_of(&id).map_or(0, Diff::rejected_hunks);
        let msg = if n > 0 {
            format!("{n} rejected hunk(s) will be dropped. {msg}")
        } else {
            msg
        };
        self.mode = Mode::Confirm {
            msg,
            action: Action::Publish(id),
        };
    }

    fn publish(&mut self, id: &str) {
        let r = self.beads.show(id);
        let Some(bead) = self.report(r) else {
            return self.mutated();
        };
        let base = self.pr_base(id);
        let msg = format!("{id}: {}", bead.title);
        let rejection = self.review_of(id).map(Diff::rejection).unwrap_or_default();
        let old = bead.label("pr").map(String::from);
        let r = self
            .git
            .commit_pending(id, &msg)
            .and_then(|_| self.git.drop_rejected(id, &self.base, &msg, &rejection))
            .and_then(|_| self.git.push(id, &self.cfg.pr.remote, old.is_some()))
            .and_then(|_| {
                pr::create(
                    &self.repo,
                    &git::branch(id),
                    &base,
                    &bead.title,
                    &pr::body(id, &bead.description),
                    self.cfg.pr.draft,
                )
            });
        let Some(created) = self.report(r) else {
            return self.mutated();
        };
        let label = format!("pr:{}", created.number);
        let r = old
            .filter(|n| *n != created.number.to_string())
            .map_or(Ok(()), |n| self.beads.remove_label(id, &format!("pr:{n}")))
            .and_then(|_| self.beads.add_label(id, &label))
            .and_then(|_| self.beads.set_state(id, State::Pr));
        if self.report(r).is_some() {
            self.git.clear_revision(id);
            self.reply_revised(id, created.number);
            self.stop_check(id);
            self.remove_agent(&agent::review_id(id), Outcome::Killed);
            self.remove_agent(id, Outcome::Killed);
            self.parked_review = None;
            self.view = View::Board;
            self.status = format!("published {id} as PR #{} {}", created.number, created.url);
            if let Some(tx) = &self.pr_refresh {
                let _ = tx.send(());
            }
        }
        self.mutated();
    }

    fn reply_revised(&mut self, id: &str, number: u64) {
        let Some(fb) = self
            .pr_watch
            .beads
            .get_mut(id)
            .and_then(|s| s.revising.take())
        else {
            return;
        };
        if let Err(e) = self.pr_watch.save() {
            tracing::warn!("pr watch: {e:#}");
        }
        if !self.cfg.pr.reply || fb.is_empty() {
            return;
        }
        let r = self
            .git
            .head(id)
            .and_then(|head| pr::post_comment(&self.repo, number, &pr::reply(&fb, &head)));
        if let Err(e) = r {
            tracing::warn!("reply on PR #{number} for {id}: {e:#}");
        }
    }

    fn on_pr(&mut self, id: &str, number: u64, r: Result<PrStatus, String>) {
        let s = match r {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("pr #{number} for {id}: {e}");
                return;
            }
        };
        let n = number.to_string();
        let Some(title) = self
            .bead(id)
            .filter(|b| {
                b.status != "closed"
                    && b.state() == Some(State::Pr)
                    && b.label("pr") == Some(n.as_str())
            })
            .map(|b| b.title.clone())
        else {
            return;
        };
        if s.state == PrState::Merged {
            return self.pr_merged(id, number);
        }
        let seen = self.pr_watch.beads.entry(id.to_string()).or_default();
        let before = seen.clone();
        let activity = seen.update(&s);
        let queued = s.state == PrState::Open && seen.queue(&activity);
        if *seen != before
            && let Err(e) = self.pr_watch.save()
        {
            tracing::warn!("pr watch: {e:#}");
        }
        if activity.is_empty() {
            return;
        }
        let notes = pr::notes(number, &activity);
        if !notes.is_empty() {
            let r = self.beads.append_notes(id, &notes);
            self.report(r);
        }
        let mut what = Vec::new();
        if activity.closed {
            what.push("closed without merging".to_string());
        }
        match activity.comments.len() {
            0 => {}
            1 => what.push("1 new comment".into()),
            c => what.push(format!("{c} new comments")),
        }
        if activity.failing.is_some() {
            what.push("checks failing".into());
        }
        if queued {
            if self.cfg.pr.auto_revise {
                let r = self
                    .beads
                    .set_state_because(id, State::Revise, "PR feedback");
                if self.report(r).is_some() {
                    what.push("queued for revision".into());
                }
                self.mutated();
            } else {
                what.push("U revises".into());
            }
        }
        let what = what.join(", ");
        self.status = format!("PR #{number} for {id}: {what}");
        self.notify(Trigger::Pr, id, &format!("{title}: PR #{number} {what}"));
    }

    fn pr_merged(&mut self, id: &str, number: u64) {
        self.stop_check(id);
        self.remove_agent(&agent::review_id(id), Outcome::Merged);
        self.remove_agent(id, Outcome::Merged);
        let children = self.git.children(id);
        let r = self.git.retire(id, &self.base);
        if self.report(r).is_none() {
            return self.mutated();
        }
        self.status = match self.close_with(id, &format!("merged via PR #{number}")) {
            Ok(()) => format!("PR #{number} merged, closed {id}"),
            Err(e) => format!("PR #{number} merged but bd close of {id} failed: {e}"),
        };
        if self.pr_watch.beads.remove(id).is_some()
            && let Err(e) = self.pr_watch.save()
        {
            tracing::warn!("pr watch: {e:#}");
        }
        self.notifier.clear(id);
        if self.viewing().as_deref() == Some(id) {
            self.parked_review = None;
            self.view = View::Board;
        }
        if !children.is_empty() {
            let onto = self
                .git
                .fetch_base(&self.cfg.pr.remote, &self.base)
                .unwrap_or_else(|e| {
                    tracing::warn!("fetch {}: {e:#}", self.base);
                    self.base.clone()
                });
            let r = self.restack_onto(children, onto);
            self.status.push_str(&r.summary());
        }
        self.mutated();
    }

    fn reviewing(&self, id: &str) -> bool {
        self.bead(id).and_then(Bead::state) == Some(State::Reviewing)
    }

    fn agent_key(&self, id: &str) -> String {
        if self.reviewing(id) {
            agent::review_id(id)
        } else {
            id.to_string()
        }
    }

    fn spawn_sandboxed(&mut self, slot: usize, launch: Launch, kind: AgentKind) -> Result<()> {
        self.sandbox.prepare(slot, &launch)?;
        let cmd = agent::command(self.backend(kind), &launch, self.cfg.kache, Some(slot));
        let cmd = self.sandbox.wrap(slot, &launch, cmd)?;
        self.insert_agent(cmd, launch.id)
    }

    fn take_agent(&mut self, id: &str) -> Option<usize> {
        if let Some(mut a) = self.agents.remove(id) {
            a.kill();
        }
        self.slots.release(id)
    }

    fn finish(&mut self, slot: Option<usize>, id: &str, outcome: Outcome) {
        let Some(slot) = slot else {
            return;
        };
        if self.slots.held(slot) {
            let d = self.deferred.entry(slot).or_insert(outcome);
            *d = d.worst(outcome);
            return;
        }
        let outcome = self
            .deferred
            .remove(&slot)
            .map_or(outcome, |d| d.worst(outcome));
        let r = self.sandbox.finished(slot, id, outcome);
        self.report(r);
    }

    fn remove_agent(&mut self, id: &str, outcome: Outcome) {
        let slot = self.take_agent(id);
        self.finish(slot, id, outcome);
    }

    fn start_chat(&mut self, resume: bool) -> Result<()> {
        let resume = resume && self.chat_session.is_some();
        let session = match &self.chat_session {
            Some(s) if resume => s.clone(),
            _ => agent::chat_session(&self.repo),
        };
        self.chat_session = Some(session.clone());
        if !self.chat_sessions.contains(&session) {
            self.chat_sessions.push(session.clone());
            self.watch_usage();
        }
        let launch = Launch {
            id: CHAT_ID.to_string(),
            session,
            bead: false,
            worktree: self.repo.clone(),
            resume,
            prompt: (!resume).then(|| agent::chat_prompt(&self.base, self.cfg.max_agents)),
            model: None,
        };
        let cmd = agent::command(self.backend(self.cfg.agent), &launch, self.cfg.kache, None);
        self.insert_agent(cmd, launch.id)
    }

    fn watch_usage(&self) {
        let now = Utc::now();
        let mut watch: Watch = self
            .all
            .iter()
            .filter(|b| b.column(now, self.cfg.done_retention_days).is_some())
            .map(|b| {
                let sessions = [b.id.clone(), agent::review_id(&b.id)]
                    .iter()
                    .map(|id| agent::session_id(&self.repo, id))
                    .collect();
                let kind = agent::selection(b, self.cfg.agent).0;
                (b.id.clone(), kind, sessions)
            })
            .collect();
        if !self.chat_sessions.is_empty() {
            watch.push((
                CHAT_ID.to_string(),
                self.cfg.agent,
                self.chat_sessions.clone(),
            ));
        }
        let _ = self.usage_watch.send(watch);
    }

    fn open_chat(&mut self, fresh: bool) {
        self.parked_review = None;
        if fresh {
            if let Some(mut a) = self.agents.remove(CHAT_ID) {
                a.kill();
            }
            self.chat_session = None;
        }
        if !self.agents.get(CHAT_ID).is_some_and(|a| a.alive) {
            self.agents.remove(CHAT_ID);
            let r = self.start_chat(true);
            if self.report(r).is_none() {
                return;
            }
        }
        self.open_progress(CHAT_ID.to_string());
        self.enter_insert(CHAT_ID);
    }

    fn insert_agent(&mut self, cmd: portable_pty::CommandBuilder, id: String) -> Result<()> {
        let (r, c) = self.pty_size();
        let log = (id != CHAT_ID)
            .then(|| agent::open_log(&self.agent_log(&id), &id).ok())
            .flatten();
        let agent = Agent::spawn(cmd, &id, r, c, log, self.tx.clone())?;
        self.agents.insert(id, agent);
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

    fn on_mouse(&mut self, m: MouseEvent) {
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
        if matches!(self.mode, Mode::Normal | Mode::Inspect { .. }) {
            let code = if up { KeyCode::Up } else { KeyCode::Down };
            self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
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
        let cols = self.columns();
        let selected: Vec<Option<String>> = cols
            .iter()
            .zip(self.rows)
            .map(|(c, r)| c.get(r).map(|b| b.id.clone()))
            .collect();
        self.filter = Filter::parse(query);
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

    fn submit(&mut self, kind: InputKind, text: String) {
        if text.is_empty() {
            return;
        }
        match kind {
            InputKind::Feedback(id) => self.feedback(&id, &text),
            InputKind::Retry(id) => self.retry(&id, &text),
            InputKind::Command => self.command(&text),
            InputKind::Search(_) => self.set_filter(&text),
            InputKind::Comment => {
                if let View::Review(d) = &mut self.view {
                    if d.add_comment(&text) {
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

    fn command(&mut self, cmd: &str) {
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

    fn create_bead(
        &mut self,
        title: &str,
        description: &str,
        priority: u8,
        backlog: bool,
        labels: &[String],
    ) {
        let r = self
            .beads
            .create(title, description, priority, backlog, labels);
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
        if id == CHAT_ID {
            self.mode = Mode::Confirm {
                msg: "End the conversation? y/n".into(),
                action: Action::Kill(id),
            };
            return;
        }
        if let Some(bead) = agent::reviewed_bead(&self.agent_key(&id)) {
            self.mode = Mode::Confirm {
                msg: format!("Stop the reviewer of {bead} and return it to Review? y/n"),
                action: Action::Kill(agent::review_id(bead)),
            };
            return;
        }
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
            Action::Publish(id) => self.publish(&id),
            Action::ResolveConflicts(id) => self.resolve_conflicts(&id),
            Action::Discard(id) => self.discard(&id),
        }
    }

    fn kill(&mut self, id: &str) {
        if agent::reviewed_bead(id).is_none() {
            self.stop_check(id);
        }
        self.remove_agent(id, Outcome::Killed);
        if id == CHAT_ID {
            self.chat_session = None;
            self.status = "conversation ended".into();
            self.view = View::Board;
            return;
        }
        if let Some(bead) = agent::reviewed_bead(id) {
            let r = self.beads.set_state(bead, State::Review);
            if self.report(r).is_some() {
                self.status = format!("stopped reviewer of {bead}, back in Review");
            }
            if matches!(self.view, View::Progress(_)) {
                self.view = View::Board;
            }
            return self.mutated();
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

    fn release_backlog(&mut self) {
        let backlog: Vec<Bead> = self
            .all
            .iter()
            .filter(|b| b.status != "closed" && b.state() == Some(State::Backlog))
            .cloned()
            .collect();
        if backlog.is_empty() {
            self.status = "backlog is empty".into();
            return;
        }
        let mut moved = 0;
        for b in &backlog {
            let r = self.beads.clear_state(b);
            if self.report(r).is_some() {
                moved += 1;
            }
        }
        if moved == backlog.len() {
            self.status = format!("released {moved} backlog bead(s) to pending");
        }
        self.mutated();
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
        self.stop_check(id);
        self.remove_agent(id, Outcome::Discarded);
        let r = self
            .git
            .discard(id)
            .and_then(|_| self.beads.set_state(id, State::Held))
            .and_then(|_| self.beads.unclaim(id));
        if self.report(r).is_some() {
            self.status = format!("discarded {id}, held in Pending (s to release)");
        }
        self.view = View::Board;
        self.mutated();
    }

    fn request_retry(&mut self, id: String) {
        if agent::reviewed_bead(&id).is_some() || id == CHAT_ID {
            return;
        }
        self.mode = Mode::Input {
            kind: InputKind::Retry(id),
            buf: String::new(),
        };
    }

    fn retry(&mut self, id: &str, note: &str) {
        self.remove_agent(id, Outcome::Discarded);
        self.failed.remove(id);
        let entry = format!("Retry after discarded attempt: {note}");
        let r = self
            .git
            .remove(id)
            .and_then(|_| self.beads.append_notes(id, &entry))
            .and_then(|_| self.beads.show(id))
            .and_then(|b| self.beads.clear_state(&b))
            .and_then(|_| self.beads.unclaim(id));
        if self.report(r).is_some() {
            self.status = format!("retrying {id} from scratch with note");
        }
        self.view = View::Board;
        self.mutated();
    }

    fn merge(&mut self, id: &str) {
        let blockers = self.beads.open_blockers(id);
        let Some(blockers) = self.report(blockers) else {
            return self.mutated();
        };
        if !blockers.is_empty() {
            self.status = format!("cannot merge {id}: blocked by open {}", blockers.join(", "));
            return;
        }
        if self.git.was_merged(id) {
            self.remove_agent(id, Outcome::Merged);
            self.status = match self.close_merged(id) {
                Ok(()) => format!("{id} was already merged, closed bead"),
                Err(e) => format!("{id} was already merged but bd close failed: {e}"),
            };
            let r = self.restack_from(self.git.children(id));
            self.status.push_str(&r.summary());
            self.view = View::Board;
            return self.mutated();
        }
        let title = self.bead(id).map(|b| b.title.clone()).unwrap_or_default();
        self.stop_check(id);
        let slot = self.take_agent(id);
        let msg = format!("{id}: {title}");
        let rejection = self.review_of(id).map(Diff::rejection).unwrap_or_default();
        let r = self
            .git
            .drop_rejected(id, &self.base, &msg, &rejection)
            .and_then(|_| self.git.merge(id, &self.base, &msg));
        let outcome = if r.is_ok() {
            Outcome::Merged
        } else {
            Outcome::Killed
        };
        self.finish(slot, id, outcome);
        if let Err(e) = &r
            && e.downcast_ref::<RebaseConflict>().is_some()
        {
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
        if self.report(r).is_some() {
            self.status = match self.close_merged(id) {
                Ok(()) => format!("merged {id} into {}", self.base),
                Err(e) => format!("merged {id} into {} but bd close failed: {e}", self.base),
            };
            let r = self.restack_from(self.git.children(id));
            self.status.push_str(&r.summary());
            self.view = View::Board;
        }
        self.mutated();
    }

    fn live(&self, id: &str) -> bool {
        [id.to_string(), agent::review_id(id)]
            .iter()
            .any(|k| self.agents.get(k).is_some_and(|a| a.alive))
    }

    fn restack_from(&mut self, queue: Vec<String>) -> Restacked {
        self.restack_onto(queue, self.base.clone())
    }

    fn restack_onto(&mut self, mut queue: Vec<String>, onto: String) -> Restacked {
        let mut r = Restacked::default();
        while let Some(c) = queue.pop() {
            let fp = self.git.fingerprint(&c);
            if self.restack_stuck.get(&c) == Some(&fp) {
                continue;
            }
            self.restack_stuck.remove(&c);
            if self.live(&c) {
                r.deferred.push(c);
                continue;
            }
            match self.git.restack(&c, &onto) {
                Ok(()) => {
                    queue.extend(self.git.children(&c));
                    r.moved.push(c);
                }
                Err(e) => {
                    tracing::warn!("restack of {c} failed: {e:#}");
                    self.restack_stuck.insert(c.clone(), fp);
                    r.stuck.push(c);
                }
            }
        }
        r
    }

    fn sweep_stacks(&mut self) {
        let stacks = self.git.stacks();
        self.restack_stuck
            .retain(|id, _| stacks.iter().any(|(c, _)| c == id));
        let mut seeds = Vec::new();
        for (child, parent) in stacks {
            if self.bead(&parent).is_none_or(|b| b.status != "closed") {
                continue;
            }
            if self.git.unmerged_branch(&parent, &self.base) {
                tracing::info!("{parent} closed without merging, unstacking {child}");
                self.git.unstack(&child);
                continue;
            }
            seeds.push(child);
        }
        if seeds.is_empty() {
            return;
        }
        let r = self.restack_from(seeds);
        if !r.moved.is_empty() || !r.stuck.is_empty() {
            self.status = r.summary().trim_start_matches(", ").to_string();
        }
    }

    fn close_merged(&self, id: &str) -> Result<()> {
        self.close_with(id, "merged by sulla")
    }

    fn close_with(&self, id: &str, reason: &str) -> Result<()> {
        self.beads.close(id, reason).or_else(|e| {
            tracing::warn!("bd close {id} failed, retrying with --force: {e:#}");
            self.beads.force_close(id, reason)
        })
    }

    fn resolve_conflicts(&mut self, id: &str) {
        let up = self.git.upstream(id, &self.base);
        let rebase = if up == self.base {
            format!("git rebase {up}")
        } else {
            format!("git rebase --onto {} {up}", self.base)
        };
        let text = format!(
            "Merging into {base} failed with conflicts. Run `{rebase}`, resolve every conflict while preserving the intent of both sides, finish the rebase, and make sure the build and tests still pass. Do not push or merge. When done, run `bd set-state {id} sulla=review --reason \"ready for review\"` then stop and wait.",
            base = self.base,
            id = id
        );
        self.feedback(id, &text);
    }

    fn feedback(&mut self, id: &str, text: &str) {
        self.ai_reviews.remove(id);
        self.deliver(id, text);
        self.status = format!("feedback sent to {id}");
        self.parked_review = None;
        self.view = View::Progress(id.to_string());
        self.mutated();
    }

    fn deliver(&mut self, id: &str, text: &str) {
        self.budget_runs.remove(id);
        let r = self.beads.set_state(id, State::Running);
        self.report(r);
        match self.agents.get_mut(id) {
            Some(a) if a.alive => {
                a.submit(text);
            }
            _ => {
                self.remove_agent(id, Outcome::Exited);
                if let Some(b) = self.bead(id).cloned() {
                    let r = self.start_agent(&b, true, Some(text.to_string()));
                    self.report(r);
                }
            }
        }
    }

    fn relaunch(&mut self, id: &str) {
        if self.agents.get(id).is_some_and(|a| a.alive) {
            return;
        }
        if agent::reviewed_bead(id).is_some() || self.reviewing(id) {
            self.status = format!("{id} is under ai review, X to stop it");
            return;
        }
        self.remove_agent(id, Outcome::Exited);
        if id == CHAT_ID {
            let r = self.start_chat(true);
            if self.report(r).is_some() {
                self.status = "conversation resumed".into();
            }
            return;
        }
        let Some(b) = self.bead(id).cloned() else {
            return;
        };
        let resume = self.git.worktree_path(id).join(".git").exists();
        if !resume
            && self.stack_on(id).is_none()
            && let Some(by) = self.blocked.get(id)
        {
            let by = by.join(", ");
            if matches!(b.state(), Some(State::Held | State::Backlog)) {
                let r = self.beads.clear_state(&b);
                self.report(r);
                self.mutated();
            }
            self.status = format!("{id} is blocked by {by}, queued in Pending");
            return;
        }
        let r = self.start_agent(&b, resume, None);
        if self.report(r).is_some() {
            self.failed.remove(id);
            if !matches!(b.state(), Some(State::Review | State::Pr)) {
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

    fn request_edit(&mut self, id: &str) {
        if id == CHAT_ID {
            return;
        }
        let path = self.git.worktree_path(id);
        if !path.join(".git").exists() {
            self.status = format!("no worktree for {id}");
            return;
        }
        self.pending_edit = Some(path);
        if self.agents.get(id).is_some_and(|a| a.alive) {
            self.status = format!("agent {id} is running, edits may conflict");
        }
    }

    pub fn edit_finished(&mut self, r: std::io::Result<std::process::ExitStatus>) {
        match r {
            Ok(s) if s.success() => {}
            Ok(s) => self.status = format!("editor exited with {s}"),
            Err(e) => self.status = format!("editor failed: {e}"),
        }
        let View::Review(d) = &mut self.view else {
            return;
        };
        let (id, sel, comments) = (d.id.clone(), d.sel, std::mem::take(&mut d.comments));
        self.reload_review(id);
        if let View::Review(d) = &mut self.view {
            d.comments = comments;
            d.sel = sel.min(d.files.len().saturating_sub(1));
        }
    }

    fn reload_review(&mut self, id: String) {
        let r = self.load_diff(&id);
        let Some(mut d) = self.report(r) else { return };
        if let View::Review(old) = &self.view
            && old.id == id
        {
            d.carry_rejections(old);
        }
        self.view = View::Review(d);
        self.inject_ai(&id);
    }

    fn load_diff(&self, id: &str) -> Result<Diff> {
        let done = self.bead(id).and_then(|b| b.column(Utc::now(), i64::MAX)) == Some(Column::Done);
        let text = if done {
            self.git.merged_diff(id)
        } else {
            self.git.diff(id, &self.base)
        }?;
        let log = if done {
            self.git.merged_log(id)
        } else {
            self.git.log(id, &self.base)
        };
        Ok(Diff::parse(id, &text).with_commits(&log.unwrap_or_default()))
    }

    fn open_review(&mut self, id: String) {
        let r = self.load_diff(&id);
        if let Some(d) = self.report(r) {
            self.view = View::Review(d);
            self.inject_ai(&id);
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
            KeyCode::Char('z') => self.zoom = !self.zoom,
            KeyCode::Esc if self.zoom => self.zoom = false,
            KeyCode::Esc if self.filter.is_active() => {
                self.set_filter("");
                self.status = "filter cleared".into();
            }
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

    fn inspect(&mut self, id: String) {
        let r = self.beads.show_text(&id);
        if let Some(mut text) = self.report(r) {
            if let Some(u) = self.usage.get(&id) {
                text = format!("{}\n\nUSAGE\n  {}\n", text.trim_end(), u.detail());
            }
            if let Some(b) = self.budget_status(&id) {
                text = format!("{}\n\nBUDGET\n  {b}\n", text.trim_end());
            }
            if let Some(m) = self.over_budget.get(&id) {
                text = format!("{}\n\nHELD\n  {m}\n", text.trim_end());
            }
            self.mode = Mode::Inspect {
                id,
                text,
                scroll: 0,
            };
        }
    }

    fn show_timeline(&mut self, id: String) {
        let r = self
            .bead(&id)
            .cloned()
            .map_or_else(|| self.beads.show(&id), Ok)
            .and_then(|b| Ok((self.beads.events(&id)?, self.beads.comments(&id)?, b)));
        if let Some((events, comments, bead)) = self.report(r) {
            let t = timeline::build(&bead, &events, &comments, Utc::now());
            self.mode = Mode::Inspect {
                id,
                text: timeline::render(&bead, &t),
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
            KeyCode::Char('p') => return self.open_progress(self.agent_key(&id)),
            KeyCode::Char('r') => return self.open_review(id),
            KeyCode::Char('l') => return self.show_log(id),
            KeyCode::Char('h') => return self.show_timeline(id),
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
                Column::Review | Column::AwaitingPr | Column::Done => self.open_review(b.id),
                _ => self.open_progress(self.agent_key(&b.id)),
            },
            KeyCode::Char('i') => self.inspect(b.id),
            KeyCode::Char('H') => self.show_timeline(b.id),
            KeyCode::Char('e') => self.request_edit(&b.id),
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
            _ => {}
        }
    }

    fn review_key(&mut self, k: KeyEvent, pending: Option<char>) {
        self.review_key_inner(k, pending);
        self.sync_ai();
    }

    fn review_key_inner(&mut self, k: KeyEvent, pending: Option<char>) {
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
                if d.comment_target().is_some() {
                    self.mode = Mode::Input {
                        kind: InputKind::Comment,
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

    pub fn shutdown(&mut self) {
        for a in self.agents.values_mut() {
            a.kill();
        }
        for c in self.checks.values_mut() {
            c.stop();
        }
        self.slots.clear();
        self.deferred.clear();
        self.sandbox.shutdown();
        self.notifier.shutdown();
    }
}
