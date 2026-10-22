use std::path::PathBuf;

use color_eyre::{
    Result,
    eyre::{bail, eyre},
};

use crate::agent;
use crate::beads::{Bead, Beads, State};
use crate::config::{PrConfig, PrTool, Trigger, Workflow};
use crate::git::{Git, RebaseConflict};
use crate::graphite;
use crate::pr::{self, PrState, PrStatus};
use crate::sandbox::Outcome;
use crate::ui::review::{Diff, Rejection};

use super::{Action, App, Event, Mode, View};

fn rebase_command(up: &str, onto: &str) -> String {
    if up == onto {
        format!("git rebase {onto}")
    } else {
        format!("git rebase --onto {onto} {up}")
    }
}

struct Publish {
    id: String,
    base: String,
    stacked: bool,
    trunk: String,
    rejection: Rejection,
    pr: PrConfig,
    repo: PathBuf,
    git: Git,
    beads: Beads,
    republish: Option<String>,
}

impl Publish {
    fn run(&self) -> Result<(Bead, pr::Pr)> {
        let id = self.id.as_str();
        let bead = self.beads.show(id)?;
        let msg = format!("{id}: {}", bead.title);
        self.git.commit_pending(id, &msg)?;
        self.git
            .drop_rejected(id, &self.trunk, &msg, &self.rejection)?;
        if self.stacked && self.git.behind_parent(id) {
            self.git.restack(id, &self.trunk)?;
        }
        let created = match self.pr.tool {
            PrTool::Gh => {
                self.git
                    .push(id, &self.pr.remote, bead.label("pr").is_some())?;
                let (title, body) = self.text(&bead, &msg)?;
                pr::create(
                    &self.repo,
                    &self.git.branch(id),
                    &self.base,
                    &title,
                    &body,
                    self.pr.draft,
                )?
            }
            PrTool::Graphite => self.submit_graphite(&bead, &msg)?,
        };
        Ok((bead, created))
    }

    fn text(&self, bead: &Bead, auto_msg: &str) -> Result<(String, String)> {
        let commit = self
            .git
            .first_commit(&self.id, &self.base)?
            .filter(|(subject, _)| subject != auto_msg);
        Ok(pr::text(&self.id, &bead.title, &bead.description, commit))
    }

    fn submit_graphite(&self, bead: &Bead, auto_msg: &str) -> Result<pr::Pr> {
        let id = self.id.as_str();
        let dir = self.git.worktree_path(id);
        let br = self.git.branch(id);
        let existed = pr::find(&self.repo, &br)?.is_some();
        self.git.sync_trunk(&self.pr.remote, &self.trunk)?;
        let submit = || {
            graphite::track(&dir, &br, &self.base)?;
            graphite::submit(&dir, self.pr.draft)
        };
        if let Err(e) = submit() {
            if !graphite::needs_rebase(&e) {
                return Err(e);
            }
            tracing::info!(
                "gt rejected {br}, rebasing onto {} and retrying: {e:#}",
                self.base
            );
            self.git.rebase(id, &self.trunk)?;
            submit()?;
        }
        let Some(created) = pr::find(&self.repo, &br)? else {
            bail!("gt submit opened no PR for {br}");
        };
        if !existed {
            let (title, body) = self.text(bead, auto_msg)?;
            pr::edit(&self.repo, created.number, &title, &body)?;
        }
        Ok(created)
    }
}

impl App {
    pub(super) fn revise(&mut self, bead: &Bead) {
        let id = bead.id.clone();
        if !self.agents.get(&id).is_some_and(|a| a.alive) && !self.make_room(&id) {
            return self.no_room(&id);
        }
        tracing::info!("revising {id} from PR feedback");
        let r = self.start_revision(bead);
        if self.report(r).is_some() {
            self.failed.remove(&id);
            self.status = format!("revising {id} from PR feedback");
            self.show_note();
        } else {
            self.revision_failed(id);
        }
    }

    fn revision_failed(&mut self, id: String) {
        self.failed.insert(id.clone());
        self.edit_local(&id, |b| b.set_state_label(Some(State::Revise)));
        self.bd_run(move |beads| beads.set_state(&id, State::Revise));
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
        self.edit_local(id, |b| b.set_state_label(Some(State::Running)));
        let bead_id = id.to_string();
        self.bd_run(move |beads| {
            beads.set_state_because(&bead_id, State::Running, "revising PR feedback")
        });
        match self.agents.get_mut(id) {
            Some(a) if a.alive => a.send_prompt(&text),
            _ => {
                self.remove_agent(id, Outcome::Exited);
                let rid = id.to_string();
                self.start_agent(bead.clone(), true, Some(text), move |app, r| {
                    if app.report(r).is_some() {
                        app.show_note();
                    } else {
                        app.revision_failed(rid);
                    }
                });
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
        let rebase = rebase_command(&up, &c.base);
        Some(format!(
            "Rebasing onto the latest {} hit conflicts. First run `{rebase}`, resolve every conflict while preserving the intent of both sides, and finish the rebase.",
            c.base
        ))
    }

    pub(super) fn request_revise(&mut self, bead: &Bead) {
        if self.refuse_released(&bead.id) {
            return;
        }
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

    pub(super) fn open_pr(&mut self, id: &str) {
        let Some(n) = self
            .bead(id)
            .and_then(|b| b.label("pr"))
            .map(str::to_string)
        else {
            self.status = format!("{id} has no PR");
            return;
        };
        let Ok(number) = n.parse::<u64>() else {
            self.status = format!("{id} has an invalid PR label pr:{n}");
            return;
        };
        self.status = format!("opening PR #{number} in the browser...");
        let repo = self.repo.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = pr::browse(&repo, number);
            let _ = tx.send(Event::Finished(Box::new(move |app: &mut App| {
                if app.report(r).is_some() {
                    app.status = format!("opened PR #{number} in the browser");
                }
            })));
        });
    }

    pub(super) fn awaiting_pr(&self, id: &str) -> bool {
        self.bead(id)
            .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Pr))
    }

    pub(super) fn released(&self, id: &str) -> bool {
        self.bead(id)
            .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Released))
    }

    pub(super) fn refuse_released(&mut self, id: &str) -> bool {
        let released = self.released(id);
        if released {
            self.status = format!(
                "{id} is released, check out {} to work on it",
                self.git.branch(id)
            );
        }
        released
    }

    fn releasable(&mut self, id: &str) -> bool {
        if self.refuse_released(id) || self.busy_publishing(id) {
            return false;
        }
        let open = self.bead(id).is_some_and(|b| {
            b.status != "closed" && matches!(b.state(), Some(State::Review | State::Pr))
        });
        if !open {
            self.status =
                format!("{id} is not in Review or Awaiting PR, only those can be released");
            return false;
        }
        if !self.git.worktree_path(id).join(".git").exists() {
            return true;
        }
        let dirty = self.git.dirty(id);
        match self.report(dirty) {
            Some(s) if s.trim().is_empty() => true,
            Some(_) => {
                self.status = format!(
                    "{id} has uncommitted changes in its worktree, commit or discard them before releasing"
                );
                false
            }
            None => false,
        }
    }

    pub(super) fn request_release(&mut self, id: String) {
        if self.released(&id) {
            return self.reclaim(&id);
        }
        if !self.releasable(&id) {
            return;
        }
        let br = self.git.branch(&id);
        self.mode = Mode::Confirm {
            msg: format!(
                "Release {id}: stop its agent and remove its worktree so you can check out {br}? y/n"
            ),
            action: Action::Release(id),
        };
    }

    pub(super) fn release_worktree(&mut self, id: &str) {
        let key = id.to_string();
        if self.stop_launch_then(id, "release it", move |app| app.release_worktree(&key)) {
            return;
        }
        if !self.releasable(id) {
            return;
        }
        let Some(bead) = self.bead(id).cloned() else {
            return;
        };
        self.stop_check(id);
        self.remove_agent(&agent::review_id(id), Outcome::Killed);
        self.remove_agent(id, Outcome::Killed);
        if self.cfg.pr.backup_sessions {
            self.backup_session(&bead);
        }
        let r = self.git.release(id);
        if self.report(r).is_none() {
            return self.mutated();
        }
        if self.parked_review.as_ref().is_some_and(|d| d.id == id) {
            self.parked_review = None;
        }
        if self.viewing().as_deref() == Some(id) {
            if matches!(self.mode, Mode::Insert) {
                self.mode = Mode::Normal;
            }
            self.view = View::Board;
        }
        self.edit_local(id, |b| b.set_state_label(Some(State::Released)));
        let bead_id = id.to_string();
        self.bd_run(move |beads| {
            beads.set_state_because(&bead_id, State::Released, "released to the user")
        });
        self.status = format!(
            "released {id}, check out {} to work on it",
            self.git.branch(id)
        );
        self.mutated();
    }

    pub(super) fn reclaim(&mut self, id: &str) {
        if self.branch_in_use(id) {
            return;
        }
        if !self.git.has_branch(id) {
            self.status = format!(
                "{} no longer exists, cannot reclaim {id}",
                self.git.branch(id)
            );
            return;
        }
        if let Some(p) = self.git.stack_parent(id) {
            let r = self.git.adopt_parent(id, &p);
            if self.report(r).is_none() {
                return;
            }
        }
        let r = self.git.reclaim(id, &self.base);
        let Some(log) = self.report(r) else {
            return self.mutated();
        };
        let state = if self.bead(id).is_some_and(|b| b.label("pr").is_some()) {
            State::Pr
        } else {
            State::Review
        };
        let note =
            (!log.is_empty()).then(|| format!("Changes made by the user while released:\n{log}"));
        self.edit_local(id, |b| {
            b.set_state_label(Some(state));
            if let Some(note) = &note {
                if !b.notes.is_empty() {
                    b.notes.push('\n');
                }
                b.notes.push_str(note);
            }
        });
        let bead_id = id.to_string();
        self.bd_run(move |beads| {
            if let Some(note) = &note {
                beads.append_notes(&bead_id, note)?;
            }
            beads.set_state_because(&bead_id, state, "reclaimed from the user")
        });
        self.status = format!("reclaimed {id}");
        match log.lines().count() {
            0 => {}
            1 => self.status.push_str(", 1 new commit"),
            n => self.status.push_str(&format!(", {n} new commits")),
        }
        let r = self.reclaim_stack(id);
        self.status.push_str(&r.summary());
        if matches!(&self.view, View::Review(d) if d.id == id) {
            self.reload_review(id.to_string());
        }
        self.mutated();
    }

    pub fn pr_found(&mut self, id: &str, number: u64) {
        if !self.released(id) || self.bead(id).is_some_and(|b| b.label("pr").is_some()) {
            return;
        }
        let label = format!("pr:{number}");
        self.edit_local(id, |b| b.labels.push(label.clone()));
        let bead_id = id.to_string();
        self.bd_run(move |beads| beads.add_label(&bead_id, &label));
        self.status = format!("found PR #{number} for released {id}, watching it");
        if let Some(tx) = &self.pr_refresh {
            let _ = tx.send(());
        }
    }

    fn pr_parent(&self, id: &str) -> Option<String> {
        self.git
            .stack_parent(id)
            .filter(|p| self.bead(p).is_some_and(Bead::has_open_pr))
    }

    fn adopt_pr_blocker(&self, id: &str, open: &[String]) {
        let [p] = open else {
            return;
        };
        if self.git.stack_parent(id).is_some()
            || !self.bead(p).is_some_and(Bead::has_open_pr)
            || !self.git.has_branch(p)
        {
            return;
        }
        match self.git.adopt_parent(id, p) {
            Ok(()) => tracing::info!("stacking {id} onto {p}, its only blocker, which has a PR"),
            Err(e) => tracing::warn!("stacking {id} onto {p}: {e:#}"),
        }
    }

    pub(super) fn pr_base(&self, id: &str) -> String {
        self.pr_parent(id)
            .map(|p| self.git.branch(&p))
            .unwrap_or_else(|| self.base.clone())
    }

    pub(super) fn request_publish(&mut self, id: String) {
        if self.refuse_released(&id) {
            return;
        }
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
        if self.busy_publishing(&id) {
            return;
        }
        self.status = format!("checking blockers of {id}...");
        let chain = self.publish_chain(&id);
        let beads = chain.clone();
        self.bd(
            move |b| beads.iter().map(|id| b.open_blockers(id)).collect(),
            move |app, r| app.confirm_publish(id, chain, r),
        );
    }

    fn stack_publishable(&self, id: &str) -> bool {
        self.bead(id)
            .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Review))
            && self.git.has_branch(id)
            && !self.publishing.contains(id)
            && !self.review_of(id).is_some_and(Diff::all_rejected)
    }

    pub(super) fn publish_chain(&self, id: &str) -> Vec<String> {
        let mut chain = vec![id.to_string()];
        while let Some(p) = chain.last().and_then(|c| self.git.stack_parent(c)) {
            if chain.contains(&p) || self.awaiting_pr(&p) || !self.stack_publishable(&p) {
                break;
            }
            chain.push(p);
        }
        chain.reverse();
        chain
    }

    pub(super) fn confirm_publish(
        &mut self,
        id: String,
        chain: Vec<String>,
        blockers: Result<Vec<Vec<String>>>,
    ) {
        if self.status == format!("checking blockers of {id}...") {
            self.status.clear();
        }
        let Some(blockers) = self.report(blockers) else {
            return;
        };
        if !matches!(self.mode, Mode::Normal) || self.focused_id().as_deref() != Some(&id) {
            return;
        }
        if chain.iter().any(|c| self.publishing.contains(c)) {
            self.status = format!(
                "{} is being published, wait for it to finish",
                chain.join(", ")
            );
            return;
        }
        if let Some(open) = blockers.first() {
            self.adopt_pr_blocker(&chain[0], open);
        }
        let parents =
            std::iter::once(self.pr_parent(&chain[0])).chain(chain.iter().cloned().map(Some));
        for ((bead, mut open), parent) in chain.iter().zip(blockers).zip(parents) {
            open.retain(|b| Some(b) != parent.as_ref());
            if open.is_empty() {
                continue;
            }
            let open = open.join(", ");
            self.status = if *bead == id {
                format!("cannot publish {id}: blocked by open {open}")
            } else {
                format!("cannot publish {id}: {bead} below it is blocked by open {open}")
            };
            return;
        }
        let base = self.pr_base(&chain[0]);
        let remote = &self.cfg.pr.remote;
        let br = self.git.branch(&id);
        let msg = match (self.cfg.pr.tool, self.bead(&id).and_then(|b| b.label("pr"))) {
            (tool, _) if chain.len() > 1 => {
                let branches = chain
                    .iter()
                    .map(|c| self.git.branch(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                match tool {
                    PrTool::Gh => {
                        format!("Push {branches} to {remote} as stacked PRs onto {base}? y/n")
                    }
                    PrTool::Graphite => {
                        format!("Submit {branches} with Graphite as a stack onto {base}? y/n")
                    }
                }
            }
            (PrTool::Gh, Some(n)) => format!("Force push {br} to {remote} and update PR #{n}? y/n"),
            (PrTool::Gh, None) => {
                format!("Push {br} to {remote} and open a PR against {base}? y/n")
            }
            (PrTool::Graphite, Some(n)) => {
                format!("Submit {br} with Graphite onto {base} and update PR #{n}? y/n")
            }
            (PrTool::Graphite, None) => {
                format!("Submit {br} with Graphite and open a PR against {base}? y/n")
            }
        };
        let n: usize = chain
            .iter()
            .map(|c| self.review_of(c).map_or(0, Diff::rejected_hunks))
            .sum();
        let msg = if n > 0 {
            format!("{n} rejected hunk(s) will be dropped. {msg}")
        } else {
            msg
        };
        self.mode = Mode::Confirm {
            msg,
            action: Action::Publish(chain),
        };
    }

    pub(super) fn publish(&mut self, chain: Vec<String>) {
        let republish = chain.last().cloned().filter(|_| self.graphite());
        self.start_publish(chain, republish);
    }

    fn start_publish(&mut self, chain: Vec<String>, republish: Option<String>) {
        if chain.is_empty() || chain.iter().any(|id| self.publishing.contains(id)) {
            return;
        }
        self.status = format!("publishing {}...", chain.join(", "));
        let mut base = self.pr_base(&chain[0]);
        let mut stacked = self.pr_parent(&chain[0]).is_some();
        let mut jobs = Vec::new();
        for id in chain {
            let rejection = self.review_of(&id).map(Diff::rejection).unwrap_or_default();
            let next = self.git.branch(&id);
            self.publishing.insert(id.clone());
            jobs.push(Publish {
                id,
                base: std::mem::replace(&mut base, next),
                stacked: std::mem::replace(&mut stacked, true),
                trunk: self.base.clone(),
                rejection,
                pr: self.cfg.pr.clone(),
                repo: self.repo.clone(),
                git: self.git.clone(),
                beads: self.beads.clone(),
                republish: republish.clone(),
            });
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut jobs = jobs.into_iter();
            while let Some(job) = jobs.next() {
                let id = job.id.clone();
                let republish = job.republish.clone();
                let r = job.run();
                let failed = r.is_err();
                let _ = tx.send(Event::Finished(Box::new(move |app| {
                    app.pushed(id, r, republish)
                })));
                if failed {
                    let rest: Vec<String> = jobs.map(|j| j.id).collect();
                    if !rest.is_empty() {
                        let _ = tx.send(Event::Finished(Box::new(move |app| {
                            app.skipped_publish(rest)
                        })));
                    }
                    return;
                }
            }
        });
    }

    fn skipped_publish(&mut self, rest: Vec<String>) {
        for id in &rest {
            self.publishing.remove(id);
        }
        self.status
            .push_str(&format!(", did not publish {}", rest.join(", ")));
        self.mutated();
    }

    pub(super) fn pushed(
        &mut self,
        id: String,
        r: Result<(Bead, pr::Pr)>,
        republish: Option<String>,
    ) {
        if let (Err(e), Some(top)) = (&r, republish)
            && let Some(c) = e.downcast_ref::<RebaseConflict>()
        {
            self.publishing.remove(&id);
            self.resolve_for_publish(&id, top, c);
            return self.mutated();
        }
        let Some((bead, created)) = self.report(r) else {
            self.publishing.remove(&id);
            return self.mutated();
        };
        let old = bead.label("pr").map(String::from);
        let label = format!("pr:{}", created.number);
        let bead_id = id.clone();
        self.bd(
            move |beads| {
                if let Some(n) = old.filter(|n| *n != created.number.to_string()) {
                    beads.remove_label(&bead_id, &format!("pr:{n}"))?;
                }
                beads.add_label(&bead_id, &label)?;
                beads.set_state(&bead_id, State::Pr)?;
                Ok(created)
            },
            move |app, r| app.published(&id, bead, r),
        );
    }

    fn resolve_for_publish(&mut self, id: &str, top: String, c: &RebaseConflict) {
        tracing::warn!("publish of {id}: {c}");
        let rebase = rebase_command(&self.git.upstream(id, &self.base), &c.base);
        let text = format!(
            "Publishing {id} failed because rebasing onto {onto} hit conflicts. Run `{rebase}`, resolve every conflict while preserving the intent of both sides, finish the rebase, and make sure the build and tests still pass. Do not push or open a PR, sulla publishes again once you are done. When done, run `bd set-state {id} sulla=review --reason \"ready for review\"` then stop and wait.",
            onto = c.base
        );
        if !self.deliver(id, &text) {
            self.status = format!(
                "publishing {id} hit conflicts with {}, and no agent slot is free to resolve them",
                c.base
            );
            return;
        }
        self.republishing.insert(id.to_string(), top);
        self.status = format!(
            "publishing {id} hit conflicts with {}, its agent is resolving them and sulla will publish again",
            c.base
        );
    }

    pub(super) fn republish_resolved(&mut self, entered: &[String]) {
        let all = &self.all;
        self.republishing
            .retain(|id, _| all.iter().any(|b| b.id == *id && b.status != "closed"));
        for id in entered {
            let Some(top) = self.republishing.remove(id) else {
                continue;
            };
            if !self.graphite() || self.awaiting_pr(&top) {
                continue;
            }
            tracing::info!("{id} resolved its publish conflicts, publishing again");
            let chain = self.publish_chain(&top);
            self.start_publish(chain, None);
        }
    }

    pub(super) fn published(&mut self, id: &str, bead: Bead, r: Result<pr::Pr>) {
        self.publishing.remove(id);
        self.republishing.remove(id);
        let republished = bead.label("pr").is_some();
        if let Some(created) = self.report(r) {
            self.git.clear_revision(id);
            self.reply_revised(id, created.number);
            self.stop_check(id);
            self.remove_agent(&agent::review_id(id), Outcome::Killed);
            self.remove_agent(id, Outcome::Killed);
            if self.cfg.pr.backup_sessions {
                self.backup_session(&bead);
            }
            if self.parked_review.as_ref().is_some_and(|d| d.id == id) {
                self.parked_review = None;
            }
            if self.viewing().as_deref() == Some(id) {
                if matches!(self.mode, Mode::Insert) {
                    self.mode = Mode::Normal;
                }
                self.view = View::Board;
            }
            self.status = format!("published {id} as PR #{} {}", created.number, created.url);
            if republished && self.graphite() {
                let children = self
                    .git
                    .children(id)
                    .into_iter()
                    .filter(|c| self.awaiting_pr(c))
                    .collect();
                let r = self.restack_synced(children, self.base.clone(), Some(id));
                self.status.push_str(&r.summary());
            }
            if let Some(tx) = &self.pr_refresh {
                let _ = tx.send(());
            }
        }
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

    pub(super) fn on_pr(&mut self, id: &str, number: u64, r: Result<PrStatus, String>) {
        let s = match r {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("pr #{number} for {id}: {e}");
                return;
            }
        };
        let n = number.to_string();
        let Some((title, released)) = self
            .bead(id)
            .filter(|b| {
                b.status != "closed"
                    && matches!(b.state(), Some(State::Pr | State::Released))
                    && b.label("pr") == Some(n.as_str())
            })
            .map(|b| (b.title.clone(), b.state() == Some(State::Released)))
        else {
            return;
        };
        if s.state == PrState::Merged {
            return self.pr_merged(id, number);
        }
        let seen = self.pr_watch.beads.entry(id.to_string()).or_default();
        let before = seen.clone();
        let activity = seen.update(&s);
        let queued = !released && s.state == PrState::Open && seen.queue(&activity);
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
            let bead = id.to_string();
            self.bd_run(move |beads| beads.append_notes(&bead, &notes));
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
            what.push("U to revise".into());
        }
        let what = what.join(", ");
        self.status = format!("PR #{number} for {id}: {what}");
        self.notify(Trigger::Pr, id, &format!("{title}: PR #{number} {what}"));
    }

    fn pr_merged(&mut self, id: &str, number: u64) {
        let key = id.to_string();
        if self.stop_launch_then(id, "retire it", move |app| app.pr_merged(&key, number)) {
            return;
        }
        self.stop_check(id);
        self.remove_agent(&agent::review_id(id), Outcome::Merged);
        self.remove_agent(id, Outcome::Merged);
        let children = self.git.children(id);
        if self.git.checked_out(id).is_none() {
            self.untrack(id);
        }
        let r = self.git.retire(id, &self.base);
        let Some(kept) = self.report(r) else {
            return self.mutated();
        };
        self.status = match self.close_with(id, &format!("merged via PR #{number}")) {
            Ok(()) => format!("PR #{number} merged, closed {id}"),
            Err(e) => format!("PR #{number} merged but bd close of {id} failed: {e}"),
        };
        if let Some(path) = kept {
            self.status.push_str(&format!(
                "; {} is checked out in {}, delete it yourself",
                self.git.branch(id),
                path.display()
            ));
        }
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
}
