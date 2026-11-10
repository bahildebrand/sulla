use chrono::Utc;
use color_eyre::{Result, eyre::eyre};

use crate::agent::{self, Launch};
use crate::beads::{Bead, Column, State};
use crate::config::Trigger;
use crate::sandbox::Outcome;
use crate::ui::review::{self, AiReview, Comment, Diff, Verdict};

use super::{App, View};

impl App {
    pub(super) fn start_review(&mut self, id: &str) {
        self.auto_rounds.remove(id);
        self.begin_review(id, None);
    }

    pub(super) fn ready_for_review(&mut self, id: &str, auto: bool) {
        if !(auto && self.cfg.ai_review.auto && self.auto_review(id)) {
            self.notify_review(id);
        }
    }

    pub(super) fn notify_review(&mut self, id: &str) {
        if self.reviewing(id) || self.is_starting(&agent::review_id(id)) {
            return;
        }
        let Some(title) = self
            .bead(id)
            .filter(|b| b.status != "closed" && b.state() == Some(State::Review))
            .map(|b| b.title.clone())
        else {
            return;
        };
        if self.review_notified.insert(id.to_string()) {
            self.notify(Trigger::Review, id, &title);
        }
    }

    fn auto_review(&mut self, id: &str) -> bool {
        if self.released(id) {
            return false;
        }
        let max = self.cfg.ai_review.max_rounds;
        let round = self.auto_rounds.get(id).copied().unwrap_or(0) + 1;
        if round > max {
            self.auto_rounds.remove(id);
            self.status =
                format!("auto ai review of {id}: stopped after {max} round(s), left in review");
            return false;
        }
        if !self.begin_review(id, Some((round, max))) {
            self.auto_rounds.remove(id);
            return false;
        }
        self.auto_rounds.insert(id.to_string(), round);
        true
    }

    fn begin_review(&mut self, id: &str, auto: Option<(u32, u32)>) -> bool {
        if self.refuse_released(id) {
            return false;
        }
        let Some(b) = self.bead(id).cloned() else {
            return false;
        };
        let rid = agent::review_id(id);
        if self.agents.get(&rid).is_some_and(|a| a.alive) || self.is_starting(&rid) {
            self.status = format!("reviewer for {id} is already running");
            return false;
        }
        if b.status == "closed" || b.state() != Some(State::Review) {
            self.status = format!("{id} is not in review");
            return false;
        }
        if self.slots.get(id).is_none() && self.running_count() >= self.cfg.max_agents {
            self.status = format!("all {} agent slots are busy", self.cfg.max_agents);
            return false;
        }
        let what = match auto {
            Some((round, max)) => format!("auto ai review of {id} (round {round}/{max})"),
            None => format!("ai review of {id}"),
        };
        self.status = format!("starting {what}...");
        self.starting.insert(rid.clone());
        let id = id.to_string();
        let failed = move |app: &mut App, id: &str| {
            if auto.is_some() {
                app.auto_rounds.remove(id);
                if app.bead(id).and_then(Bead::state) == Some(State::Review) {
                    app.notify_review(id);
                }
            }
        };
        self.bd(
            move |beads| Ok(beads.show(&b.id).unwrap_or(b)),
            move |app, r| {
                app.starting.remove(&rid);
                if !app
                    .bead(&id)
                    .is_some_and(|b| b.state() == Some(State::Review))
                {
                    app.status = format!("{id} left review, ai review not started");
                    return failed(app, &id);
                }
                match r {
                    Ok(b) => app.launch_review(&b, move |app, r| {
                        if app.report(r).is_some() {
                            app.status = format!("{what} started (p to watch)");
                        } else {
                            failed(app, &id);
                        }
                    }),
                    Err(e) => {
                        app.report(Err::<(), _>(e));
                        failed(app, &id);
                    }
                }
            },
        );
        true
    }

    pub(super) fn launch_review(
        &mut self,
        bead: &Bead,
        done: impl FnOnce(&mut App, Result<()>) + Send + 'static,
    ) {
        let worktree = self.git.worktree_path(&bead.id);
        if !worktree.join(".git").exists() {
            return done(self, Err(eyre!("no worktree for {}", bead.id)));
        }
        let rid = agent::review_id(&bead.id);
        self.remove_agent(&rid, Outcome::Killed);
        self.ai_reviews.remove(&bead.id);
        self.inject_ai(&bead.id);
        let Some(slot) = self.slots.acquire_with(&rid, &bead.id, self.cfg.max_agents) else {
            return done(self, Err(eyre!("no free agent slot for {rid}")));
        };
        let (kind, model) = agent::selection(bead, self.cfg.agent);
        let b = bead.clone();
        let git = self.git.clone();
        let base = self.base.clone();
        let repo = self.repo.clone();
        let system_prompt = self.cfg.system_prompt.clone();
        let app_note = agent::app_note(&self.cfg);
        let session = rid.clone();
        let build = move || {
            let prompt = agent::with_app(
                agent::review_prompt(&b, &git.branch(&b.id), &git.upstream(&b.id, &base)),
                app_note.as_deref(),
            );
            let launch = Launch {
                session: agent::session_id(&repo, &session),
                id: session,
                bead: true,
                worktree,
                resume: false,
                prompt: Some(agent::with_system(prompt, system_prompt.as_deref())),
                model,
            };
            Ok((launch, None))
        };
        let id = bead.id.clone();
        self.launch(rid, slot, kind, build, move |app, r| {
            if r.is_ok() {
                app.edit_local(&id, |b| b.set_state_label(Some(State::Reviewing)));
                let bead = id.clone();
                app.bd_run(move |beads| beads.set_state(&bead, State::Reviewing));
            }
            done(app, r);
        });
    }

    pub(super) fn collect_ai_review(&mut self, id: &str, notify: bool) {
        let bead = id.to_string();
        let id = id.to_string();
        self.bd(
            move |beads| beads.latest_comment(&bead),
            move |app, r| {
                app.apply_ai_review(&id, r);
                if app.ai_reviews.get(&id).is_some_and(|r| r.forwarded) {
                    return;
                }
                app.auto_rounds.remove(&id);
                if notify {
                    app.notify_review(&id);
                }
            },
        );
    }

    fn apply_ai_review(&mut self, id: &str, comment: Result<Option<String>>) {
        let text = match comment {
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
            Some(json) if self.deliver(id, &json) => {
                self.status = format!("ai review of {id}: {n} comment(s) sent to agent");
            }
            Some(_) => {
                if let Some(r) = self.ai_reviews.get_mut(id) {
                    r.forwarded = false;
                }
                self.status = format!(
                    "ai review of {id}: {n} comment(s) not sent, no free agent slot (S sends them)"
                );
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

    pub(super) fn sync_ai(&mut self) {
        let View::Review(d) = &self.view else {
            return;
        };
        if let Some(r) = self.ai_reviews.get_mut(&d.id) {
            r.comments = d.comments.iter().filter(|c| c.ai).cloned().collect();
        }
    }

    pub(super) fn review_of(&self, id: &str) -> Option<&Diff> {
        let open = match &self.view {
            View::Review(d) => Some(d),
            _ => None,
        };
        [open, self.parked_review.as_ref()]
            .into_iter()
            .flatten()
            .find(|d| d.id == id)
    }

    pub(super) fn reload_review(&mut self, id: String) {
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

    pub(super) fn open_review(&mut self, id: String) {
        let r = self.load_diff(&id);
        if let Some(d) = self.report(r) {
            self.view = View::Review(d);
            self.inject_ai(&id);
        }
    }
}
