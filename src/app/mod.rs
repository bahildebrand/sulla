mod actions;
mod agents;
mod checking;
mod input;
mod launch;
mod merge;
mod monitor;
mod publish;
mod reviews;
mod schedule;
mod views;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant};

use chrono::Utc;
use color_eyre::Result;
use crossterm::event::{Event as CEvent, KeyEventKind};

use crate::agent::{self, Agent, Backend, CHAT_ID, Claude, Copilot, Stall};
use crate::beads::{Bead, Beads, Column, State};
use crate::budget;
use crate::checks::{self, Check};
use crate::config::{AgentKind, Config, Trigger, Workflow};
use crate::conflicts::Conflict;
use crate::filter::Filter;
use crate::git::{DiffStat, Git};
use crate::health::Tool;
use crate::notify::Notifier;
use crate::pr::{self, PrStatus};
use crate::presets;
use crate::sandbox::{self, Outcome, Sandbox, Slots};
use crate::scheduler::{self, Backoff, Snapshot};
use crate::ui::review::{AiReview, Diff};
use crate::usage::{self, Usage, Watch};

use launch::Launching;

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
    Inspected(String, Result<String>),
    Finished(Callback),
}

pub type Callback = Box<dyn FnOnce(&mut App) + Send>;
type BdJob = Box<dyn FnOnce(&Beads) -> Callback + Send>;

fn spawn_bd_worker(beads: Beads, tx: Sender<Event>) -> Sender<BdJob> {
    let (jobs, rx) = channel::<BdJob>();
    std::thread::spawn(move || {
        for job in rx {
            if tx.send(Event::Finished(job(&beads))).is_err() {
                return;
            }
        }
    });
    jobs
}

pub enum Suspend {
    Editor(PathBuf),
    Shell {
        cmd: std::process::Command,
        place: String,
    },
}

impl Suspend {
    pub fn name(&self) -> &'static str {
        match self {
            Suspend::Editor(_) => "editor",
            Suspend::Shell { .. } => "shell",
        }
    }
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
    Comment(usize),
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
    Publish(Vec<String>),
    ResolveConflicts(String),
    Discard(String),
    Delete(String),
    Release(String),
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
    Help {
        scroll: u16,
    },
    Health,
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
    pub stacked_on: HashMap<String, String>,
    pub dependents: HashMap<String, scheduler::Dependents>,
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
    pub chain_filter: Option<String>,
    pub rows: [usize; Column::ALL.len()],
    pub status: String,
    rebuilt: Option<String>,
    pending: Option<char>,
    pub count: Option<usize>,
    esc_at: Option<Instant>,
    last_mutation: Instant,
    tx: Sender<Event>,
    refresh: Sender<()>,
    bd_jobs: Sender<BdJob>,
    bd_pending: usize,
    starting: HashSet<String>,
    launching: HashMap<String, Launching>,
    publishing: HashSet<String>,
    republishing: HashMap<String, String>,
    term: (u16, u16),
    pub suspend: Option<Suspend>,
    pub paused: bool,
    pub health: Option<Vec<Tool>>,
    pub bd_error: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(cfg: Config, repo: PathBuf, tx: Sender<Event>) -> Result<Self> {
        let sandbox = sandbox::build(&cfg, &repo)?;
        let git = Git::new(repo.clone()).with_prefix(&cfg.branch_prefix);
        git.ensure_ignored()?;
        let base = match &cfg.base_branch {
            Some(b) => b.clone(),
            None => git.current_branch()?,
        };
        let beads = Beads::new(&repo);
        beads.connect();
        let started = Instant::now();
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
                git.clone(),
                Duration::from_secs(cfg.pr.poll_secs.max(1)),
                tx.clone(),
            )
        });
        let pr_watch = pr::Watch::load(repo.join(".sulla").join("pr-watch.json"));
        let bd_jobs = spawn_bd_worker(beads.clone(), tx.clone());
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
            stacked_on: HashMap::new(),
            dependents: HashMap::new(),
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
            chain_filter: None,
            rows: [0; Column::ALL.len()],
            status: String::new(),
            rebuilt: None,
            pending: None,
            count: None,
            esc_at: None,
            last_mutation: started,
            tx,
            refresh,
            bd_jobs,
            bd_pending: 0,
            starting: HashSet::new(),
            launching: HashMap::new(),
            publishing: HashSet::new(),
            republishing: HashMap::new(),
            term: (80, 24),
            suspend: None,
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
        if let Some(id) = &self.chain_filter {
            let chain = self.chain(id);
            for c in &mut cols {
                c.retain(|b| b.id == *id || chain.contains(&b.id));
            }
        }
        cols
    }

    pub fn is_filtered(&self) -> bool {
        self.filter.is_active() || self.chain_filter.is_some()
    }

    pub fn chain(&self, id: &str) -> scheduler::Chain {
        scheduler::chain(
            id,
            &self.all,
            &self.blocked,
            &self.stacked_on,
            &self.dependents,
        )
    }

    pub fn selected_chain(&self) -> Option<(String, scheduler::Chain)> {
        let id = self.selected()?.id;
        let chain = self.chain(&id);
        Some((id, chain))
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

    pub(super) fn is_working(&self, id: &str) -> bool {
        id != CHAT_ID
            && self.agents.get(id).is_some_and(|a| a.alive)
            && (agent::reviewed_bead(id).is_some()
                || self.bead(id).is_none_or(|b| {
                    b.status != "closed"
                        && !matches!(
                            b.state(),
                            Some(State::Reviewing | State::Review | State::Pr | State::Released)
                        )
                }))
    }

    pub fn running_count(&self) -> usize {
        self.agents.keys().filter(|id| self.is_working(id)).count()
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

    fn bd<T: Send + 'static>(
        &mut self,
        job: impl FnOnce(&Beads) -> Result<T> + Send + 'static,
        done: impl FnOnce(&mut App, Result<T>) + Send + 'static,
    ) {
        let job: BdJob = Box::new(move |beads| {
            let r = job(beads);
            Box::new(move |app: &mut App| {
                app.bd_pending -= 1;
                done(app, r);
                app.mutated();
            })
        });
        match self.bd_jobs.send(job) {
            Ok(()) => self.bd_pending += 1,
            Err(_) => self.status = "error: bd worker stopped".into(),
        }
    }

    fn bd_run(&mut self, job: impl FnOnce(&Beads) -> Result<()> + Send + 'static) {
        self.bd(job, |app, r| {
            app.report(r);
        });
    }

    fn edit_local(&mut self, id: &str, f: impl FnOnce(&mut Bead)) {
        if let Some(b) = self.all.iter_mut().find(|b| b.id == id) {
            f(b);
        }
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
                if self.bd_pending > 0 {
                    return;
                }
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
                self.stacked_on = s.stacked_on;
                self.dependents = scheduler::dependents(&self.all, &self.blocked, &self.stacked_on);
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
                self.republish_resolved(&entered);
                self.clamp();
                self.watch_usage();
                self.reconcile();
                for id in &finished {
                    self.collect_ai_review(id, entered.contains(id));
                }
                for id in entered {
                    if finished.contains(&id) {
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
            Event::Inspected(id, r) => self.on_inspected(id, r),
            Event::Finished(f) => f(self),
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

    pub fn shutdown(&mut self) {
        for a in self.agents.values_mut() {
            a.kill();
        }
        for (id, l) in self.launching.drain() {
            if l.job.join().is_err() {
                tracing::warn!("launch of {id} panicked during shutdown");
            }
        }
        for c in self.checks.values_mut() {
            c.stop();
        }
        self.slots.clear();
        self.deferred.clear();
        self.sandbox.shutdown();
        self.notifier.shutdown();
        self.beads.shutdown();
    }
}

#[cfg(test)]
mod tests;
