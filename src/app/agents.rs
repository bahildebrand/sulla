use color_eyre::Result;

use crate::agent::{self, Agent, CHAT_ID, Launch};
use crate::beads::{Bead, State};
use crate::config::AgentKind;
use crate::sandbox::Outcome;

use super::{App, Mode, View};

impl App {
    pub(super) fn reviewing(&self, id: &str) -> bool {
        self.bead(id).and_then(Bead::state) == Some(State::Reviewing)
    }

    pub(super) fn agent_key(&self, id: &str) -> String {
        let rid = agent::review_id(id);
        if self.reviewing(id) || self.launching.contains_key(&rid) {
            rid
        } else {
            id.to_string()
        }
    }

    pub(super) fn spawn_agent(
        &mut self,
        slot: usize,
        launch: Launch,
        kind: AgentKind,
    ) -> Result<()> {
        let cmd = agent::command(self.backend(kind), &launch, self.cfg.kache, Some(slot));
        let cmd = self.sandbox.wrap(slot, &launch, cmd)?;
        self.insert_agent(cmd, launch.id, launch.prompt)
    }

    pub(super) fn take_agent(&mut self, id: &str) -> Option<usize> {
        if let Some(mut a) = self.agents.remove(id) {
            a.kill();
        }
        if self.launching.contains_key(id) {
            return None;
        }
        self.slots.release(id)
    }

    pub(super) fn finish(&mut self, slot: Option<usize>, id: &str, outcome: Outcome) {
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

    pub(super) fn remove_agent(&mut self, id: &str, outcome: Outcome) {
        if outcome != Outcome::Exited
            && let Some(l) = self.launching.get_mut(id)
        {
            l.stopped = true;
        }
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
            prompt: (!resume).then(|| {
                agent::with_system(
                    agent::chat_prompt(&self.base, self.cfg.max_agents),
                    self.cfg.system_prompt.as_deref(),
                )
            }),
            model: None,
        };
        let cmd = agent::command(self.backend(self.cfg.agent), &launch, self.cfg.kache, None);
        self.insert_agent(cmd, launch.id, launch.prompt)
    }

    pub(super) fn open_chat(&mut self, fresh: bool) {
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

    fn insert_agent(
        &mut self,
        cmd: portable_pty::CommandBuilder,
        id: String,
        prompt: Option<String>,
    ) -> Result<()> {
        let (r, c) = self.pty_size();
        let log = (id != CHAT_ID)
            .then(|| agent::open_log(&self.agent_log(&id), &id).ok())
            .flatten();
        let mut agent = Agent::spawn(cmd, &id, r, c, log, self.tx.clone())?;
        if let Some(p) = prompt {
            agent.retry_prompt(p);
        }
        self.agents.insert(id, agent);
        Ok(())
    }

    pub(super) fn write_focused(&mut self, bytes: &[u8]) {
        if let View::Progress(id) = &self.view
            && let Some(a) = self.agents.get_mut(id)
        {
            a.write(bytes);
        }
    }

    pub(super) fn focused_app_cursor(&self) -> (bool, bool) {
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

    pub(super) fn live(&self, id: &str) -> bool {
        [id.to_string(), agent::review_id(id)]
            .iter()
            .any(|k| self.agents.get(k).is_some_and(|a| a.alive) || self.launching.contains_key(k))
    }

    pub(super) fn deliver(&mut self, id: &str, text: &str) -> bool {
        let alive = self.agents.get(id).is_some_and(|a| a.alive);
        if !alive {
            self.remove_agent(id, Outcome::Exited);
            if !self.make_room(id) {
                self.no_room(id);
                return false;
            }
        }
        let was = self.bead(id).and_then(Bead::state);
        self.budget_runs.remove(id);
        self.edit_local(id, |b| b.set_state_label(Some(State::Running)));
        let bead = id.to_string();
        self.bd_run(move |beads| beads.set_state(&bead, State::Running));
        if alive {
            if let Some(a) = self.agents.get_mut(id) {
                a.send_prompt(text);
            }
        } else if let Some(b) = self.bead(id).cloned() {
            let id = id.to_string();
            self.start_agent(b, true, Some(text.to_string()), move |app, r| {
                if app.report(r).is_some() {
                    app.rebuilt_status();
                } else {
                    app.undeliver(&id, was);
                }
            });
        }
        true
    }

    fn undeliver(&mut self, id: &str, was: Option<State>) {
        let now = self.bead(id).and_then(Bead::state);
        if was != Some(State::Review) || now != Some(State::Running) || self.live(id) {
            return;
        }
        self.edit_local(id, |b| b.set_state_label(was));
        let bead = id.to_string();
        self.bd_run(move |beads| beads.set_state(&bead, State::Review));
    }

    pub(super) fn relaunch(&mut self, id: &str) {
        if self.agents.get(id).is_some_and(|a| a.alive) {
            return;
        }
        if self.is_starting(id) {
            self.status = format!("{id} is already starting");
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
                self.edit_local(id, |b| b.set_state_label(None));
                self.bd_run(move |beads| beads.clear_state(&b));
            }
            self.status = format!("{id} is blocked by {by}, queued in Pending");
            return;
        }
        if !self.make_room(id) {
            return self.no_room(id);
        }
        let run = !matches!(b.state(), Some(State::Review | State::Pr));
        self.status = format!("launching {id}...");
        let id = id.to_string();
        self.start_agent(b, resume, None, move |app, r| {
            if app.report(r).is_none() {
                return;
            }
            app.failed.remove(&id);
            if run {
                app.edit_local(&id, |b| b.set_state_label(Some(State::Running)));
                let bead = id.clone();
                app.bd_run(move |beads| beads.set_state(&bead, State::Running));
            }
            app.status = format!("launched {id}");
            app.rebuilt_status();
        });
    }

    pub(super) fn enter_insert(&mut self, id: &str) {
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
}
