use std::collections::{BTreeMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;

use color_eyre::{Result, eyre::eyre};

use crate::agent::{self, Launch};
use crate::beads::{Bead, State};
use crate::config::{AgentKind, Workflow};
use crate::git::Git;
use crate::pr;
use crate::presets;
use crate::sandbox::{self, Outcome};
use crate::scheduler;
use crate::sessions::{self, Resume};
use crate::setup::{self, Setup};

use super::{App, Callback, Event};

pub(super) struct Launching {
    was: Option<State>,
    pub(super) stopped: bool,
    after: Vec<Callback>,
    pub(super) job: JoinHandle<()>,
}

type Built = (Launch, Option<String>);

static WORKTREES: Mutex<()> = Mutex::new(());

struct Recover {
    root: PathBuf,
    kind: AgentKind,
    in_pr: HashSet<String>,
}

struct Prelaunch {
    bead: Bead,
    resume: bool,
    prompt: Option<String>,
    model: Option<String>,
    onto: Option<String>,
    base: String,
    repo: PathBuf,
    git: Git,
    presets: BTreeMap<String, String>,
    system_prompt: Option<String>,
    recover: Option<Recover>,
    setup: Option<Setup>,
}

impl Prelaunch {
    fn run(mut self) -> Result<Built> {
        let id = self.bead.id.as_str();
        let worktree = {
            let _git = WORKTREES.lock().unwrap_or_else(PoisonError::into_inner);
            let fresh =
                !self.git.worktree_path(id).join(".git").exists() && !self.git.has_branch(id);
            match self
                .onto
                .as_deref()
                .filter(|p| fresh && self.git.has_branch(p))
            {
                Some(parent) => self.git.ensure_stacked_worktree(id, parent)?,
                None => self.git.ensure_worktree(id, &self.base)?,
            }
        };
        let ran = self.setup.take().and_then(|s| self.run_setup(s));
        let parent = self.git.stack_parent(id);
        let prompt = self
            .prompt
            .clone()
            .or_else(|| (!self.resume).then(|| self.task(parent.as_deref())));
        let prompt = setup::with_note(prompt, ran.as_ref());
        let (resume, prompt, rebuilt) = match &self.recover {
            Some(r) => self.recover(r, parent.as_deref(), prompt),
            None => (self.resume, prompt, None),
        };
        let failed = ran.filter(|r| !r.ok).map(|r| r.summary(id));
        let note = match (rebuilt, failed) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        let launch = Launch {
            session: agent::session_id(&self.repo, id),
            id: id.to_string(),
            bead: true,
            worktree,
            resume,
            prompt,
            model: self.model.clone(),
        };
        Ok((launch, note))
    }

    fn run_setup(&self, s: Setup) -> Option<setup::Ran> {
        let id = self.bead.id.as_str();
        if !self.git.needs_setup(id) {
            return None;
        }
        tracing::info!("{id}: running setup `{}`", s.script);
        let ran = s.run(id, false);
        tracing::info!("{}", ran.summary(id));
        if let Err(e) = self.git.mark_setup(id) {
            tracing::warn!("{id}: marking setup done: {e:#}");
        }
        Some(ran)
    }

    fn task(&self, parent: Option<&str>) -> String {
        let preset = presets::for_bead(&self.presets, &self.bead);
        let branch = self.git.branch(&self.bead.id);
        let parent = parent.map(|p| (p, self.git.branch(p)));
        let parent = parent.as_ref().map(|(p, b)| (*p, b.as_str()));
        agent::with_system(
            agent::task_prompt(&self.bead, &branch, parent, preset),
            self.system_prompt.as_deref(),
        )
    }

    fn recover(
        &self,
        r: &Recover,
        parent: Option<&str>,
        prompt: Option<String>,
    ) -> (bool, Option<String>, Option<String>) {
        let id = &self.bead.id;
        let session = agent::session_id(&self.repo, id);
        let dest = sessions::backup_dir(&self.repo, id);
        match sessions::ensure(&r.root, r.kind, &session, &dest) {
            Ok(Resume::Live) => return (true, prompt, None),
            Ok(Resume::Restored) => {
                tracing::info!("restored session of {id} from {}", dest.display());
                return (true, prompt, None);
            }
            Ok(Resume::Lost) => {}
            Err(e) => {
                tracing::warn!("restoring session of {id}: {e:#}");
                return (true, prompt, None);
            }
        }
        tracing::warn!("session of {id} is gone and has no backup, rebuilding it");
        let task = self.task(parent);
        let pr = self.bead.label("pr").map(|n| {
            n.parse()
                .ok()
                .and_then(|n| pr::url(&self.repo, n).ok())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| format!("PR #{n}"))
        });
        let base = parent
            .filter(|p| r.in_pr.contains(*p))
            .map(|p| self.git.branch(p))
            .unwrap_or_else(|| self.base.clone());
        let log = self.git.summary(id, &base).unwrap_or_else(|e| {
            tracing::warn!("git log for {id}: {e:#}");
            String::new()
        });
        let text = sessions::rebuild_prompt(&task, pr.as_deref(), &log, prompt.as_deref());
        let msg = format!(
            "{id}: agent session was lost, rebuilt a fresh one from the bead, PR and git log"
        );
        (false, Some(text), Some(msg))
    }
}

fn join_notes(id: &str, note: Option<String>, warnings: Vec<String>) -> Option<String> {
    let warnings = (!warnings.is_empty()).then(|| format!("{id}: {}", warnings.join("; ")));
    match (note, warnings) {
        (Some(n), Some(w)) => Some(format!("{n}; {w}")),
        (n, w) => n.or(w),
    }
}

fn stopped(id: &str) -> color_eyre::Report {
    eyre!("{id} was stopped before its agent started")
}

impl App {
    pub(super) fn start_agent(
        &mut self,
        bead: Bead,
        resume: bool,
        prompt: Option<String>,
        done: impl FnOnce(&mut App, Result<()>) + Send + 'static,
    ) {
        let id = bead.id.clone();
        let was = self.bead(&id).and_then(Bead::state);
        self.starting.insert(id.clone());
        self.bd(
            move |beads| Ok(beads.show(&bead.id).unwrap_or(bead)),
            move |app, r| {
                app.starting.remove(&id);
                match r {
                    Ok(_) if app.launch_stopped(&id, was) => done(app, Err(stopped(&id))),
                    Ok(b) => app.launch_or_prompt(&b, resume, prompt, done),
                    Err(e) => done(app, Err(e)),
                }
            },
        );
    }

    fn launch_stopped(&self, id: &str, was: Option<State>) -> bool {
        if let Some(bead) = agent::reviewed_bead(id) {
            return !self
                .bead(bead)
                .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Review));
        }
        self.bead(id).is_none_or(|b| {
            let now = b.state();
            b.status == "closed"
                || (now != was && matches!(now, Some(State::Held | State::Backlog)))
        })
    }

    fn launch_or_prompt(
        &mut self,
        bead: &Bead,
        resume: bool,
        prompt: Option<String>,
        done: impl FnOnce(&mut App, Result<()>) + Send + 'static,
    ) {
        if self.launching.contains_key(&bead.id) {
            let was = self.bead(&bead.id).and_then(Bead::state);
            let b = bead.clone();
            self.after_launch(&bead.id, false, move |app| {
                if app.launch_stopped(&b.id, was) {
                    done(app, Err(stopped(&b.id)));
                } else {
                    app.launch_or_prompt(&b, resume, prompt, done);
                }
            });
            return;
        }
        match self.agents.get_mut(&bead.id) {
            Some(a) if a.alive => {
                if let Some(p) = prompt {
                    a.send_prompt(&p);
                }
                done(self, Ok(()));
            }
            _ => self.launch_agent(bead, resume, prompt, done),
        }
    }

    pub(super) fn launch_agent(
        &mut self,
        bead: &Bead,
        resume: bool,
        prompt: Option<String>,
        done: impl FnOnce(&mut App, Result<()>) + Send + 'static,
    ) {
        let id = bead.id.clone();
        let (kind, model) = agent::selection(bead, self.cfg.agent);
        let Some(slot) = self
            .slots
            .acquire_with(&id, &agent::review_id(&id), self.cfg.max_agents)
        else {
            return done(self, Err(eyre!("no free agent slot for {id}")));
        };
        let on_pr = self.cfg.workflow == Workflow::Pr;
        let recover = (resume && on_pr)
            .then(|| self.session_root(kind))
            .flatten()
            .map(|root| Recover {
                root,
                kind,
                in_pr: self
                    .all
                    .iter()
                    .filter(|b| {
                        b.status != "closed"
                            && matches!(b.state(), Some(State::Pr | State::Released))
                    })
                    .map(|b| b.id.clone())
                    .collect(),
            });
        let job = Prelaunch {
            bead: bead.clone(),
            resume,
            prompt,
            model,
            onto: scheduler::stack_parent(&id, &self.blocked, &self.all, on_pr).map(String::from),
            base: self.base.clone(),
            repo: self.repo.clone(),
            git: self.git.clone(),
            presets: self.cfg.presets.clone(),
            system_prompt: self.cfg.system_prompt.clone(),
            recover,
            setup: self.setup_job(&id, Some(slot)),
        };
        self.launch(id, slot, kind, move || job.run(), done);
    }

    pub(super) fn launch(
        &mut self,
        id: String,
        slot: usize,
        kind: AgentKind,
        build: impl FnOnce() -> Result<Built> + Send + 'static,
        done: impl FnOnce(&mut App, Result<()>) + Send + 'static,
    ) {
        let prepare = self.sandbox.prepare(slot, &id);
        let tx = self.tx.clone();
        let key = id.clone();
        let job = std::thread::spawn(move || {
            let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let warnings = prepare()?;
                let (launch, note) = build()?;
                Ok((launch, join_notes(&key, note, warnings)))
            }))
            .unwrap_or_else(|_| Err(eyre!("launching {key} panicked")));
            let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| {
                app.launched(&key, slot, kind, r, done)
            })));
        });
        let was = self
            .bead(agent::reviewed_bead(&id).unwrap_or(&id))
            .and_then(Bead::state);
        self.launching.insert(
            id,
            Launching {
                was,
                stopped: false,
                after: Vec::new(),
                job,
            },
        );
    }

    pub(super) fn launched(
        &mut self,
        id: &str,
        slot: usize,
        kind: AgentKind,
        r: Result<Built>,
        done: impl FnOnce(&mut App, Result<()>),
    ) {
        let Some(l) = self.launching.remove(id) else {
            return;
        };
        let r = if l.stopped || self.launch_stopped(id, l.was) {
            Err(stopped(id))
        } else {
            r.and_then(|(launch, note)| {
                self.spawn_agent(slot, launch, kind)?;
                if let Some(m) = note {
                    self.status = m.clone();
                    self.note = Some(m);
                }
                Ok(())
            })
        };
        if r.is_err() {
            self.slots.release(id);
            self.finish(Some(slot), id, Outcome::Exited);
        }
        done(self, r);
        for f in l.after {
            f(self);
        }
        self.mutated();
    }

    fn after_launch(
        &mut self,
        id: &str,
        stop: bool,
        f: impl FnOnce(&mut App) + Send + 'static,
    ) -> bool {
        let Some(l) = self.launching.get_mut(id) else {
            return false;
        };
        l.stopped |= stop;
        l.after.push(Box::new(f));
        true
    }

    pub(super) fn stop_launch_then(
        &mut self,
        id: &str,
        what: &str,
        f: impl FnOnce(&mut App) + Send + 'static,
    ) -> bool {
        if !self.after_launch(id, true, f) {
            return false;
        }
        self.status = format!("{id} is still launching, will {what} once it stops");
        true
    }

    fn setup_job(&self, id: &str, slot: Option<usize>) -> Option<Setup> {
        let script = self.cfg.hooks.setup_script()?;
        let dir = self.git.worktree_path(id);
        let argv = setup::argv(script);
        let env = setup::env(id, slot, self.cfg.kache);
        let cmd = match slot {
            Some(slot) => self.sandbox.command(slot, &dir, &argv, &env),
            None => sandbox::host_command(&dir, &argv, &env),
        };
        Some(Setup {
            script: script.to_string(),
            cmd,
            log: self.agent_log(id),
        })
    }

    pub(super) fn rerun_setup(&mut self, id: &str) {
        if self.cfg.hooks.setup_script().is_none() {
            self.status = "no setup hook, set `[hooks] setup` in .sulla/config.toml".into();
            return;
        }
        if !self.git.worktree_path(id).join(".git").exists() {
            self.status = format!("{id} has no worktree");
            return;
        }
        if self.setups.contains(id) || self.launching.contains_key(id) {
            self.status = format!("{id} is already setting up");
            return;
        }
        let slot = self.slots.get(id);
        if slot.is_none() && self.sandbox.isolated() {
            self.status = match self.git.clear_setup(id) {
                Ok(()) => {
                    format!("{id} has no slot container, setup runs when its agent next starts")
                }
                Err(e) => format!("{id}: {e:#}"),
            };
            return;
        }
        let Some(job) = self.setup_job(id, slot) else {
            return;
        };
        self.setups.insert(id.to_string());
        self.status = format!("{id}: running setup `{}`", job.script);
        let (git, tx, key) = (self.git.clone(), self.tx.clone(), id.to_string());
        std::thread::spawn(move || {
            let ran = job.run(&key, false);
            if let Err(e) = git.mark_setup(&key) {
                tracing::warn!("{key}: marking setup done: {e:#}");
            }
            let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| {
                app.setups.remove(&key);
                app.status = ran.summary(&key);
            })));
        });
    }

    fn session_root(&self, kind: AgentKind) -> Option<PathBuf> {
        let host = sessions::Roots::from_env();
        self.sandbox.session_root(kind, host.get(kind))
    }

    pub(super) fn backup_session(&self, bead: &Bead) {
        let (kind, _) = agent::selection(bead, self.cfg.agent);
        let Some(root) = self.session_root(kind) else {
            return;
        };
        let session = agent::session_id(&self.repo, &bead.id);
        let dest = sessions::backup_dir(&self.repo, &bead.id);
        match sessions::backup(&root, kind, &session, &dest) {
            Ok(true) => tracing::info!("backed up session of {} to {}", bead.id, dest.display()),
            Ok(false) => tracing::warn!(
                "no {} session {session} to back up for {}",
                kind.name(),
                bead.id
            ),
            Err(e) => tracing::warn!("backing up session of {}: {e:#}", bead.id),
        }
    }

    pub(super) fn show_note(&mut self) {
        if let Some(m) = self.note.take() {
            self.status = m;
        }
    }
}
