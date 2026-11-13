mod actions;
mod agents;
mod checking;
mod config_edit;
mod input;
mod launch;
mod merge;
mod monitor;
mod publish;
mod reload;
mod reviews;
mod schedule;
mod views;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chrono::Utc;
use color_eyre::Result;
use crossterm::event::{Event as CEvent, KeyEventKind};

use crate::agent::{self, Agent, Backend, CHAT_ID, Claude, Copilot, Stall};
use crate::beads::{Bead, Beads, Column, State};
use crate::browse;
use crate::budget;
use crate::checks::{self, Check};
use crate::config::effective::{Layers, Source};
use crate::config::reload::{self as config_reload, Watch as ConfigWatch};
use crate::config::{AgentKind, Config, Trigger, Workflow};
use crate::conflicts::Conflict;
use crate::filter::Filter;
use crate::git::{DiffStat, Git};
use crate::health::Tool;
use crate::notify::{self, Notifier};
use crate::pr::{self, PrStatus};
use crate::presets;
use crate::sandbox::{self, Outcome, Sandbox, Slots};
use crate::scheduler::{self, Backoff, Interval, Snapshot};
use crate::ui::config::ConfigView;
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
type GitJob = Box<dyn FnOnce() -> Callback + Send>;

fn backends(cfg: &Config) -> HashMap<AgentKind, Box<dyn Backend>> {
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
    backends
}

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

fn spawn_git_worker(tx: Sender<Event>) -> (Sender<GitJob>, JoinHandle<()>) {
    let (jobs, rx) = channel::<GitJob>();
    let worker = std::thread::spawn(move || {
        for job in rx {
            if tx.send(Event::Finished(job())).is_err() {
                return;
            }
        }
    });
    (jobs, worker)
}

pub enum Suspend {
    Editor(PathBuf),
    File(PathBuf),
    Shell {
        cmd: std::process::Command,
        place: String,
    },
}

impl Suspend {
    pub fn name(&self) -> &'static str {
        match self {
            Suspend::Editor(_) | Suspend::File(_) => "editor",
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
    pub acceptance: String,
    pub priority: u8,
    pub agent: Option<AgentKind>,
    pub presets: Vec<String>,
    pub preset: Option<usize>,
    pub backlog: bool,
    pub field: usize,
}

impl NewBeadForm {
    pub const FIELDS: usize = 7;

    pub fn new(presets: Vec<String>) -> Self {
        Self {
            title: String::new(),
            description: String::new(),
            acceptance: String::new(),
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
    ReleaseBacklog,
    RestartApp(String),
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
        config: Option<Box<ConfigView>>,
    },
    Health,
    Config(Box<ConfigView>),
    Inspect {
        id: String,
        text: String,
        scroll: u16,
    },
}

const ESC_TIMEOUT: Duration = Duration::from_millis(300);
const CHIME: Duration = Duration::from_secs(5);
const ERROR_FRESH: Duration = Duration::from_secs(10);
pub const ERROR: &str = "error: ";

pub fn is_error(status: &str) -> bool {
    status.starts_with(ERROR) || status.starts_with(config_reload::FAILED)
}

pub struct App {
    pub cfg: Config,
    pub layers: Layers,
    pub reload: config_reload::State,
    config_watch: ConfigWatch,
    config_edit: Option<Source>,
    poll_every: Interval,
    pr_every: Interval,
    pub base: String,
    repo: PathBuf,
    chat_session: Option<String>,
    chat_sessions: Vec<String>,
    review_sessions: HashMap<String, Vec<String>>,
    bead_sessions: HashMap<String, Vec<String>>,
    beads: Beads,
    git: Git,
    backends: HashMap<AgentKind, Box<dyn Backend>>,
    sandbox: Box<dyn Sandbox>,
    slots: Slots,
    published_slots: Option<(BTreeMap<String, usize>, browse::Apps)>,
    apps: browse::Apps,
    app_live: HashMap<usize, browse::Live>,
    probing: bool,
    probed: Option<Instant>,
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
    focused: Option<bool>,
    last_input: Instant,
    chime: Option<(String, Instant)>,
    error_at: Option<Instant>,
    pub pr_watch: pr::Watch,
    pr_refresh: Option<Sender<()>>,
    pub stalls: HashMap<String, Stall>,
    waits: HashMap<String, Instant>,
    review_notified: HashSet<String>,
    stall_checked: Option<Instant>,
    budget_runs: HashMap<String, budget::Run>,
    budget_checked: Option<Instant>,
    pub over_budget: HashMap<String, String>,
    states: Option<HashMap<String, Option<State>>>,
    pub view: View,
    pub parked_review: Option<Diff>,
    saved_reviews: HashMap<String, Diff>,
    pub ai_reviews: HashMap<String, AiReview>,
    auto_rounds: HashMap<String, u32>,
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
    note: Option<String>,
    setups: HashSet<String>,
    pending: Option<char>,
    pub count: Option<usize>,
    esc_at: Option<Instant>,
    last_mutation: Instant,
    tx: Sender<Event>,
    refresh: Sender<()>,
    bd_jobs: Sender<BdJob>,
    bd_pending: usize,
    background_pending: usize,
    git_jobs: Sender<GitJob>,
    git_worker: Option<JoinHandle<()>>,
    git_pending: usize,
    merging: HashSet<String>,
    sweeping: bool,
    starting: HashSet<String>,
    launching: HashMap<String, Launching>,
    publishing: HashSet<String>,
    browsing: HashSet<String>,
    browsed: HashMap<String, Instant>,
    republishing: HashMap<String, String>,
    term: (u16, u16),
    pub suspend: Option<Suspend>,
    pub paused: bool,
    pub health: Option<Vec<Tool>>,
    pub bd_error: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(cfg: Config, layers: Layers, repo: PathBuf, tx: Sender<Event>) -> Result<Self> {
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
        let poll_every = Interval::new(cfg.poll_secs);
        let pr_every = Interval::new(cfg.pr.poll_secs);
        let refresh = scheduler::spawn_poller(
            beads.clone(),
            git.clone(),
            base.clone(),
            poll_every.clone(),
            tx.clone(),
        );
        let pr_refresh = (cfg.workflow == Workflow::Pr).then(|| {
            scheduler::spawn_pr_watcher(beads.clone(), git.clone(), pr_every.clone(), tx.clone())
        });
        let pr_watch = pr::Watch::load(repo.join(".sulla").join("pr-watch.json"));
        let bd_jobs = spawn_bd_worker(beads.clone(), tx.clone());
        let (git_jobs, git_worker) = spawn_git_worker(tx.clone());
        let usage_watch = usage::spawn(poll_every.clone(), tx.clone());
        let backends = backends(&cfg);
        let config_watch = ConfigWatch::new(layers.paths());
        Ok(Self {
            notifier: Notifier::new(cfg.notify.clone()),
            focused: None,
            last_input: started,
            chime: None,
            error_at: None,
            pr_watch,
            pr_refresh,
            stalls: HashMap::new(),
            waits: HashMap::new(),
            review_notified: HashSet::new(),
            stall_checked: None,
            budget_runs: HashMap::new(),
            budget_checked: None,
            over_budget: HashMap::new(),
            states: None,
            cfg,
            layers,
            reload: config_reload::State::default(),
            config_watch,
            poll_every,
            pr_every,
            base,
            repo,
            chat_session: None,
            chat_sessions: Vec::new(),
            review_sessions: HashMap::new(),
            bead_sessions: git.sessions(),
            beads,
            git,
            backends,
            sandbox,
            slots: Slots::default(),
            published_slots: None,
            apps: browse::Apps::default(),
            app_live: HashMap::new(),
            probing: false,
            probed: None,
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
            saved_reviews: HashMap::new(),
            ai_reviews: HashMap::new(),
            auto_rounds: HashMap::new(),
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
            note: None,
            setups: HashSet::new(),
            pending: None,
            count: None,
            esc_at: None,
            last_mutation: started,
            tx,
            refresh,
            bd_jobs,
            bd_pending: 0,
            background_pending: 0,
            git_jobs,
            git_worker: Some(git_worker),
            git_pending: 0,
            merging: HashSet::new(),
            sweeping: false,
            starting: HashSet::new(),
            launching: HashMap::new(),
            publishing: HashSet::new(),
            browsing: HashSet::new(),
            browsed: HashMap::new(),
            republishing: HashMap::new(),
            term: (80, 24),
            suspend: None,
            config_edit: None,
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
        let mut loc = self.sandbox.instance(slot)?;
        if let Some(ip) = self.sandbox.address(slot) {
            loc.push_str(&format!(" {ip}"));
        }
        if let Some(ip) = self.sandbox.loopback(slot) {
            loc.push_str(&format!(" host {ip}"));
        }
        Some(loc)
    }

    pub fn app_status(&self, id: &str) -> Option<(browse::Live, Option<&str>)> {
        let slot = self.slots.get(id)?;
        let live = *self.app_live.get(&slot)?;
        let shown = agent::reviewed_bead(id).unwrap_or(id);
        Some((live, self.apps.stale(slot, shown)))
    }

    fn sync_apps(&mut self) {
        let base = self
            .published_slots
            .as_ref()
            .map(|(_, a)| a.clone())
            .unwrap_or_default();
        let theirs = browse::read_own_apps(&self.repo);
        self.apps = self.apps.merge(&base, &theirs);
    }

    fn publish_slots(&mut self) {
        let slots = self.slots.snapshot();
        if self
            .published_slots
            .as_ref()
            .is_some_and(|(s, a)| *s == slots && *a == self.apps)
        {
            return;
        }
        self.sync_apps();
        if let Err(e) = browse::write_slots(&self.repo, &slots, &self.apps) {
            tracing::warn!("writing slot map failed: {e:#}");
        }
        self.published_slots = Some((slots, self.apps.clone()));
    }

    fn probe_apps(&mut self) {
        let every = Duration::from_secs(self.cfg.poll_secs.max(1));
        if self.probing || self.probed.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        self.probed = Some(Instant::now());
        let ports = self.cfg.incus.browse.ports.clone();
        let mut held: Vec<usize> = self.slots.snapshot().into_values().collect();
        held.sort_unstable();
        held.dedup();
        let targets: Vec<(usize, String)> = held
            .into_iter()
            .filter_map(|s| self.sandbox.loopback(s).map(|ip| (s, ip)))
            .collect();
        if ports.is_empty() || targets.is_empty() {
            self.app_live.clear();
            return;
        }
        self.probing = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let live = browse::probe(&targets, &ports);
            let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| {
                app.probing = false;
                app.app_live = live.into_iter().collect();
                app.sync_apps();
            })));
        });
    }

    pub(super) fn browse(&mut self, args: &str) {
        let mut words = args.split_whitespace();
        let (kind, target) = match words.next() {
            Some("stop") => (browse::Kind::Stop, words.next()),
            Some("restart") => (browse::Kind::Restart, words.next()),
            w => (browse::Kind::Open, w),
        };
        let target = match target {
            Some(t) => t.to_string(),
            None => match self.viewing().or_else(|| self.selected().map(|b| b.id)) {
                Some(id) => id,
                None => return self.status = "browse: no bead selected".into(),
            },
        };
        self.start_browse(&target, kind);
    }

    pub(super) fn start_browse(&mut self, target: &str, kind: browse::Kind) {
        if target.parse::<usize>().is_ok() || self.holds_slot(target) {
            return self.open_browse(target, kind);
        }
        if self.bead(target).is_none() {
            return self.status = format!("browse: no bead {target}");
        }
        if kind == browse::Kind::Stop {
            return self.status = format!("browse: {target} has no slot, so no app to stop");
        }
        if let Err(e) = browse::supported(&self.cfg) {
            return self.status = format!("browse: {e:#}");
        }
        if !self.git.worktree_path(target).join(".git").exists() {
            return self.status =
                format!("browse: {target} has no slot or worktree, press A to start it");
        }
        let then = target.to_string();
        self.relaunch_then(target, move |app| app.open_browse(&then, kind));
    }

    fn holds_slot(&self, id: &str) -> bool {
        self.slots.get(id).is_some() || self.slots.get(&agent::review_id(id)).is_some()
    }

    fn open_browse(&mut self, target: &str, kind: browse::Kind) {
        let job = match browse::job(
            &self.cfg,
            &self.repo,
            target,
            &self.slots.snapshot(),
            kind,
            Some(self.sandbox.as_ref()),
        ) {
            Ok(j) => j,
            Err(e) => return self.status = format!("browse: {e:#}"),
        };
        let label = job.label.clone();
        if self.browsing.contains(&label) {
            return self.status = format!("browse {label}: already in progress");
        }
        if let Some(other) = job.stale(&self.apps) {
            self.mode = Mode::Confirm {
                msg: format!("{} y/n", job.restart_question(other)),
                action: Action::RestartApp(target.to_string()),
            };
            return;
        }
        self.browsing.insert(label.clone());
        if kind != browse::Kind::Stop
            && let Some(bead) = &job.bead
        {
            self.browsed.insert(bead.clone(), Instant::now());
        }
        self.status = format!("browse {label}: checking the app…");
        let (slot, bead) = (job.slot, job.bead.clone().unwrap_or_default());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let note = |n: browse::Note| {
                let (label, bead) = (label.clone(), bead.clone());
                let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| match n {
                    browse::Note::Status(m) => app.status = format!("browse {label}: {m}"),
                    browse::Note::Up => app.apps.started(slot, &bead),
                    browse::Note::Down => app.apps.stopped(slot),
                })));
            };
            let r = job.run(&mut |n| note(n));
            let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| {
                app.browsing.remove(&label);
                app.status = match r {
                    Ok(m) => m,
                    Err(e) => format!("browse {label}: {e}").replace('\n', " | "),
                };
            })));
        });
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

    pub fn visible_columns(&self) -> Vec<usize> {
        let totals = self.all_columns().map(|c| c.len());
        (0..Column::ALL.len())
            .filter(|&i| {
                Column::ALL[i] != Column::AwaitingPr
                    || self.cfg.workflow == Workflow::Pr
                    || totals[i] > 0
            })
            .collect()
    }

    pub(super) fn step_column(&mut self, forward: bool) {
        let shown = self.visible_columns();
        let next = if forward {
            shown.iter().find(|&&i| i > self.col)
        } else {
            shown.iter().rev().find(|&&i| i < self.col)
        };
        if let Some(&i) = next {
            self.col = i;
        }
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
            Err(_) => self.fail("bd worker stopped"),
        }
    }

    fn background<T: Send + 'static>(
        &mut self,
        job: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        self.background_pending += 1;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = job();
            let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| {
                app.background_pending -= 1;
                done(app, r);
            })));
        });
    }

    fn git_job<T: Send + 'static>(
        &mut self,
        job: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        let job: GitJob = Box::new(move || {
            let r = job();
            Box::new(move |app: &mut App| {
                app.git_pending -= 1;
                done(app, r);
            })
        });
        self.git_pending += 1;
        if let Err(e) = self.git_jobs.send(job) {
            tracing::warn!("git worker stopped, running its job inline");
            (e.0)()(self);
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
            if b.state() != Some(State::Review) {
                self.review_notified.remove(id);
            }
        }
    }

    fn report<T>(&mut self, r: Result<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("{e:#}");
                self.fail(e);
                None
            }
        }
    }

    pub fn handle(&mut self, ev: Event) {
        self.dispatch(ev);
        self.watch_config();
        self.check_stalls();
        self.check_budgets();
        self.publish_slots();
        if let Some(id) = self.viewing() {
            self.notifier.clear(&id);
        }
        self.notifier.flush(Instant::now());
    }

    fn viewing(&self) -> Option<String> {
        match &self.view {
            View::Board => None,
            View::Progress(id) => Some(agent::reviewed_bead(id).unwrap_or(id).to_string()),
            View::Review(d) => Some(d.id.clone()),
        }
    }

    fn notify(&mut self, trigger: Trigger, id: &str, title: &str) {
        if self.viewing().as_deref() == Some(id) {
            return;
        }
        let now = Instant::now();
        let present = self.present(now);
        if self.notifier.fire(trigger, id, title, present, now) && present {
            let msg = format!("{} {}", notify::BELL_GLYPH, notify::line(trigger, id));
            self.chime = Some((msg, now));
        }
    }

    fn present(&self, now: Instant) -> bool {
        notify::present(
            self.focused,
            self.notifier.idle(),
            now.saturating_duration_since(self.last_input),
        )
    }

    fn fail(&mut self, e: impl std::fmt::Display) {
        self.set_error(format!("{ERROR}{e}"));
    }

    pub(super) fn set_error(&mut self, status: String) {
        self.status = status;
        self.error_at = Some(Instant::now());
    }

    pub fn fresh_error(&self) -> bool {
        is_error(&self.status)
            && self
                .error_at
                .is_some_and(|at| at >= self.last_input && at.elapsed() < ERROR_FRESH)
    }

    pub fn chime(&self) -> Option<&str> {
        self.chime
            .as_ref()
            .filter(|(_, at)| at.elapsed() < CHIME)
            .map(|(m, _)| m.as_str())
    }

    fn on_input(&mut self, now: Instant) {
        let away = !self.present(now);
        self.focused = self.focused.map(|_| true);
        self.last_input = now;
        self.returned(away, now);
    }

    fn on_focus(&mut self, focused: bool, now: Instant) {
        let away = !self.present(now);
        self.focused = Some(focused);
        if focused {
            self.last_input = now;
        }
        self.returned(away, now);
    }

    fn returned(&mut self, away: bool, now: Instant) {
        if away
            && self.present(now)
            && let Some(s) = self.notifier.welcome_back()
        {
            self.status = s;
        }
    }

    pub fn resumed(&mut self) {
        self.last_input = Instant::now();
        self.focused = self.focused.map(|_| true);
    }

    fn on_bell(&mut self, id: &str) {
        if id == CHAT_ID {
            return self.notify(Trigger::Chat, CHAT_ID, "conversation");
        }
        if agent::reviewed_bead(id).is_none() && self.awaiting_input(id) {
            self.waits.insert(id.to_string(), Instant::now());
        }
    }

    fn dispatch(&mut self, ev: Event) {
        match ev {
            Event::Input(CEvent::Key(k)) if k.kind != KeyEventKind::Release => {
                self.on_input(Instant::now());
                self.on_key(k);
            }
            Event::Input(CEvent::Paste(s)) => {
                self.on_input(Instant::now());
                self.on_paste(&s);
            }
            Event::Input(CEvent::Resize(w, h)) => self.on_resize(w, h),
            Event::Input(CEvent::Mouse(m)) => {
                self.on_input(Instant::now());
                self.on_mouse(m);
            }
            Event::Input(CEvent::FocusGained) => self.on_focus(true, Instant::now()),
            Event::Input(CEvent::FocusLost) => self.on_focus(false, Instant::now()),
            Event::Input(_) | Event::AgentOutput => {}
            Event::Tick => {
                if self.esc_at.is_some_and(|t| t.elapsed() >= ESC_TIMEOUT) {
                    self.esc_at = None;
                    self.write_focused(&[0x1b]);
                }
                self.check_rate_limits();
                self.probe_apps();
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
                    self.reviewer_exited(&id);
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
                        || all
                            .iter()
                            .any(|b| b.id == id && b.status != "closed" && flag_holds(t, b))
                });
                self.review_notified.retain(|id| {
                    all.iter().any(|b| {
                        b.id == *id && b.status != "closed" && b.state() == Some(State::Review)
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
                self.auto_rounds
                    .retain(|id, _| all.iter().any(|b| b.id == *id && b.status != "closed"));
                self.review_sessions
                    .retain(|id, _| all.iter().any(|b| b.id == *id));
                let entered = self.entered_review();
                let ids: Vec<String> = entered.iter().map(|(id, _)| id.clone()).collect();
                let republished = self.republish_resolved(&ids);
                self.clamp();
                self.watch_usage();
                self.reconcile();
                for id in &finished {
                    self.collect_ai_review(id, ids.contains(id));
                }
                for (id, from) in entered {
                    if finished.contains(&id) {
                        continue;
                    }
                    let auto = from == Some(State::Running) && !republished.contains(&id);
                    self.ready_for_review(&id, auto);
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
        let shown = self.visible_columns();
        if !shown.contains(&self.col) {
            let before = shown.iter().rev().find(|&&i| i < self.col);
            if let Some(&i) = before.or(shown.first()) {
                self.col = i;
            }
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
        self.git_jobs = channel().0;
        if let Some(w) = self.git_worker.take()
            && w.join().is_err()
        {
            tracing::warn!("git worker panicked during shutdown");
        }
        for c in self.checks.values_mut() {
            c.stop();
        }
        self.slots.clear();
        self.publish_slots();
        self.deferred.clear();
        self.sandbox.shutdown();
        self.notifier.shutdown();
        self.beads.shutdown();
    }
}

fn flag_holds(t: Trigger, b: &Bead) -> bool {
    match t {
        Trigger::Review => b.state() == Some(State::Review),
        Trigger::Waiting | Trigger::Dead | Trigger::Stalled => {
            matches!(b.state(), Some(State::Running | State::Reviewing))
        }
        Trigger::Budget | Trigger::Pr | Trigger::Chat => true,
    }
}

#[cfg(test)]
mod tests;
