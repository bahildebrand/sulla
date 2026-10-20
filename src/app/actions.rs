use color_eyre::eyre::bail;

use crate::agent::{self, CHAT_ID};
use crate::beads::{Bead, State};
use crate::sandbox::{self, Outcome};

use super::merge::Restacked;
use super::{Action, App, InputKind, Mode, Suspend, View};

impl App {
    pub(super) fn create_bead(
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

    pub(super) fn request_quit(&mut self) {
        let live = self.agents.values().filter(|a| a.alive).count();
        let launching = self.launching.len();
        let mut msg = Vec::new();
        if live > 0 {
            msg.push(format!(
                "{live} live agent(s) will be stopped and resumed next launch."
            ));
        }
        if launching > 0 {
            msg.push(format!(
                "Quitting waits for {launching} agent launch(es) in progress."
            ));
        }
        if msg.is_empty() {
            self.quit = true;
        } else {
            self.mode = Mode::Confirm {
                action: Action::Quit,
                msg: format!("{} Quit? y/n", msg.join(" ")),
            };
        }
    }

    pub(super) fn confirm_kill(&mut self, id: String) {
        if self.refuse_released(&id) {
            return;
        }
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

    pub(super) fn confirm(&mut self, action: Action) {
        match action {
            Action::Quit => self.quit = true,
            Action::Kill(id) => self.kill(&id),
            Action::Merge(id) => self.merge(&id),
            Action::Publish(chain) => self.publish(chain),
            Action::ResolveConflicts(id) => self.resolve_conflicts(&id),
            Action::Discard(id) => self.discard(&id),
            Action::Delete(id) => self.delete(&id),
            Action::Release(id) => self.release_worktree(&id),
        }
    }

    pub(super) fn kill(&mut self, id: &str) {
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
        if let Some(bead) = agent::reviewed_bead(id).map(String::from) {
            self.edit_local(&bead, |b| b.set_state_label(Some(State::Review)));
            self.status = format!("stopped reviewer of {bead}, back in Review");
            self.bd_run(move |b| b.set_state(&bead, State::Review));
            if matches!(self.view, View::Progress(_)) {
                self.view = View::Board;
            }
            return;
        }
        self.hold(id);
        self.status = format!("killed {id}, held in Pending (s to release)");
        if matches!(self.view, View::Progress(_)) {
            self.view = View::Board;
        }
    }

    fn hold(&mut self, id: &str) {
        self.edit_local(id, |b| {
            b.set_state_label(Some(State::Held));
            b.status = "open".into();
        });
        let id = id.to_string();
        self.bd_run(move |b| {
            b.set_state(&id, State::Held)?;
            b.unclaim(&id)
        });
    }

    pub(super) fn release(&mut self, bead: &Bead) {
        self.failed.remove(&bead.id);
        if matches!(bead.state(), Some(State::Held | State::Backlog)) {
            self.edit_local(&bead.id, |b| b.set_state_label(None));
            self.status = format!("released {}", bead.id);
            let bead = bead.clone();
            self.bd_run(move |b| b.clear_state(&bead));
        }
    }

    pub(super) fn release_backlog(&mut self) {
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
        for b in &backlog {
            self.edit_local(&b.id, |b| b.set_state_label(None));
        }
        self.status = format!("released {} backlog bead(s) to pending", backlog.len());
        self.bd_run(move |beads| {
            let failed: Vec<String> = backlog
                .iter()
                .filter_map(|b| beads.clear_state(b).err())
                .map(|e| format!("{e:#}"))
                .collect();
            match failed.first() {
                None => Ok(()),
                Some(e) => bail!("{} of {} not released: {e}", failed.len(), backlog.len()),
            }
        });
    }

    pub(super) fn move_to_backlog(&mut self, bead: &Bead) {
        if self.refuse_released(&bead.id) {
            return;
        }
        if self.agents.contains_key(&bead.id)
            || self.is_starting(&bead.id)
            || bead.status == "closed"
        {
            return;
        }
        self.edit_local(&bead.id, |b| b.set_state_label(Some(State::Backlog)));
        self.status = format!("{} moved to backlog (s to release)", bead.id);
        let id = bead.id.clone();
        self.bd_run(move |b| b.set_state(&id, State::Backlog));
    }

    pub(super) fn mark_review(&mut self, id: &str) {
        if self.refuse_released(id) {
            return;
        }
        self.edit_local(id, |b| b.set_state_label(Some(State::Review)));
        self.status = format!("{id} ready for review");
        let id = id.to_string();
        self.bd_run(move |b| b.set_state(&id, State::Review));
    }

    pub(super) fn busy_publishing(&mut self, id: &str) -> bool {
        let busy = self.publishing.contains(id);
        if busy {
            self.status = format!("{id} is being published, wait for it to finish");
        }
        busy
    }

    pub(super) fn branch_in_use(&mut self, id: &str) -> bool {
        let Some(path) = self.git.checked_out(id) else {
            return false;
        };
        self.status = format!(
            "{} is checked out in {}, switch off it first",
            self.git.branch(id),
            path.display()
        );
        true
    }

    pub(super) fn confirm_discard(&mut self, id: String) {
        if self.busy_publishing(&id) || self.branch_in_use(&id) {
            return;
        }
        self.mode = Mode::Confirm {
            msg: format!("Discard {id}: kill agent, delete worktree and branch, hold bead? y/n"),
            action: Action::Discard(id),
        };
    }

    pub(super) fn discard(&mut self, id: &str) {
        let key = id.to_string();
        if self.stop_launch_then(id, "discard it", move |app| app.discard(&key)) {
            return;
        }
        self.stop_check(id);
        self.remove_agent(id, Outcome::Discarded);
        let children = self.git.children(id);
        self.untrack(id);
        let r = self.git.discard(id);
        if self.report(r).is_some() {
            self.hold(id);
            self.status = format!("discarded {id}, held in Pending (s to release)");
        }
        let mut r = Restacked::default();
        self.sync_graphite(&mut r, &children, None);
        self.status.push_str(&r.summary());
        self.view = View::Board;
        self.mutated();
    }

    pub(super) fn confirm_delete(&mut self, id: String) {
        if self.busy_publishing(&id) || self.branch_in_use(&id) {
            return;
        }
        self.mode = Mode::Confirm {
            msg: format!(
                "Delete {id} permanently: kill agent, delete worktree, branch and bead? y/n"
            ),
            action: Action::Delete(id),
        };
    }

    fn delete(&mut self, id: &str) {
        let key = id.to_string();
        if self.stop_launch_then(id, "delete it", move |app| app.delete(&key)) {
            return;
        }
        self.stop_check(id);
        self.remove_agent(id, Outcome::Discarded);
        self.failed.remove(id);
        let children = self.git.children(id);
        self.untrack(id);
        let r = self.git.discard(id);
        if self.report(r).is_some() {
            self.all.retain(|b| b.id != id);
            self.clamp();
            self.status = format!("deleted {id}");
            let id = id.to_string();
            self.bd_run(move |b| b.delete(&id));
        }
        let mut r = Restacked::default();
        self.sync_graphite(&mut r, &children, None);
        self.status.push_str(&r.summary());
        self.view = View::Board;
        self.mutated();
    }

    pub(super) fn request_retry(&mut self, id: String) {
        if agent::reviewed_bead(&id).is_some()
            || id == CHAT_ID
            || self.busy_publishing(&id)
            || self.branch_in_use(&id)
        {
            return;
        }
        self.mode = Mode::Input {
            kind: InputKind::Retry(id),
            buf: String::new(),
        };
    }

    pub(super) fn retry(&mut self, id: &str, note: &str) {
        let (key, text) = (id.to_string(), note.to_string());
        if self.stop_launch_then(id, "retry it", move |app| app.retry(&key, &text)) {
            return;
        }
        self.remove_agent(id, Outcome::Discarded);
        self.failed.remove(id);
        let entry = format!("Retry after discarded attempt: {note}");
        let children = self.git.children(id);
        self.untrack(id);
        let r = self.git.remove(id);
        if self.report(r).is_some() {
            self.edit_local(id, |b| {
                b.set_state_label(None);
                b.status = "open".into();
            });
            self.status = format!("retrying {id} from scratch with note");
            let id = id.to_string();
            self.bd_run(move |b| {
                b.append_notes(&id, &entry)?;
                b.clear_state(&b.show(&id)?)?;
                b.unclaim(&id)
            });
        }
        let mut r = Restacked::default();
        self.sync_graphite(&mut r, &children, None);
        self.status.push_str(&r.summary());
        self.view = View::Board;
        self.mutated();
    }

    pub(super) fn feedback(&mut self, id: &str, text: &str) {
        if !self.deliver(id, text) {
            return;
        }
        self.ai_reviews.remove(id);
        self.status = format!("feedback sent to {id}");
        self.parked_review = None;
        self.view = View::Progress(id.to_string());
    }

    pub(super) fn request_edit(&mut self, id: &str) {
        if id == CHAT_ID || self.refuse_released(id) {
            return;
        }
        let path = self.git.worktree_path(id);
        if !path.join(".git").exists() {
            self.status = format!("no worktree for {id}");
            return;
        }
        self.suspend = Some(Suspend::Editor(path));
        if self.agents.get(id).is_some_and(|a| a.alive) {
            self.status = format!("agent {id} is running, edits may conflict");
        }
    }

    pub(super) fn request_shell(&mut self, id: &str) {
        if id == CHAT_ID || self.refuse_released(id) {
            return;
        }
        let dir = self.git.worktree_path(id);
        if !dir.join(".git").exists() {
            self.status = format!("no worktree for {id}");
            return;
        }
        let mut env = vec![("SULLA_BEAD".to_string(), id.to_string())];
        if self.cfg.kache {
            env.push(("RUSTC_WRAPPER".into(), "kache".into()));
        }
        let slot = self.slots.get(id);
        let (cmd, place) = match slot.and_then(|s| Some((s, self.sandbox.instance(s)?))) {
            Some((slot, name)) => (
                self.sandbox.shell(slot, &dir, &env),
                format!("shell in {name} at {}", dir.display()),
            ),
            None => (
                sandbox::host_shell(&dir, &env),
                format!("shell in {}", dir.display()),
            ),
        };
        if self.sandbox.isolated() && slot.is_none() {
            self.status = format!("no container for {id}, shell runs on the host");
        } else if self.agents.get(id).is_some_and(|a| a.alive) {
            self.status = format!("agent {id} is running, edits may conflict");
        }
        self.suspend = Some(Suspend::Shell { cmd, place });
    }

    pub(super) fn set_priority(&mut self, id: &str, priority: u8) {
        self.edit_local(id, |b| b.priority = priority);
        let id = id.to_string();
        self.bd_run(move |b| b.set_priority(&id, priority));
    }
}
