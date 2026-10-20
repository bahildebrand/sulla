use color_eyre::Result;

use crate::beads::Bead;
use crate::config::{PrTool, Workflow};
use crate::git::RebaseConflict;
use crate::graphite;
use crate::sandbox::Outcome;
use crate::sessions;
use crate::ui::review::Diff;

use super::{Action, App, Mode, View};

#[derive(Default)]
pub(super) struct Restacked {
    moved: Vec<String>,
    deferred: Vec<String>,
    stuck: Vec<String>,
    resubmitted: Vec<String>,
    gt_failed: Vec<String>,
}

impl Restacked {
    pub(super) fn summary(&self) -> String {
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
        if !self.resubmitted.is_empty() {
            s.push_str(&format!(
                ", resubmitted {} with Graphite",
                self.resubmitted.join(", ")
            ));
        }
        if !self.gt_failed.is_empty() {
            s.push_str(&format!(
                ", could not update Graphite for {}",
                self.gt_failed.join(", ")
            ));
        }
        s
    }
}

impl App {
    pub(super) fn request_merge(&mut self, id: String, force: bool) {
        if self.refuse_released(&id) {
            return;
        }
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

    pub(super) fn merge(&mut self, id: &str) {
        let key = id.to_string();
        if self.stop_launch_then(id, "merge it", move |app| app.merge(&key)) {
            return;
        }
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
            self.untrack(id);
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
            .and_then(|_| self.git.land(id, &self.base, &msg))
            .and_then(|_| {
                self.untrack(id);
                self.git.remove(id)
            });
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

    fn restack_from(&mut self, queue: Vec<String>) -> Restacked {
        self.restack_onto(queue, self.base.clone())
    }

    pub(super) fn restack_onto(&mut self, queue: Vec<String>, onto: String) -> Restacked {
        self.restack_synced(queue, onto, None)
    }

    pub(super) fn restack_synced(
        &mut self,
        mut queue: Vec<String>,
        onto: String,
        published: Option<&str>,
    ) -> Restacked {
        let mut r = Restacked::default();
        while let Some(c) = queue.pop() {
            if self.released(&c) {
                continue;
            }
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
        self.sync_graphite(&mut r, &[], published);
        r
    }

    pub(super) fn graphite(&self) -> bool {
        self.cfg.workflow == Workflow::Pr && self.cfg.pr.tool == PrTool::Graphite
    }

    pub(super) fn untrack(&self, id: &str) {
        if !self.graphite() {
            return;
        }
        if let Err(e) = graphite::untrack(&self.repo, &self.git.branch(id)) {
            tracing::debug!("gt untrack {id}: {e:#}");
        }
    }

    pub(super) fn sync_graphite(
        &self,
        r: &mut Restacked,
        unstacked: &[String],
        published: Option<&str>,
    ) {
        if !self.graphite() {
            return;
        }
        let owned = |id: &&String| !self.released(id);
        let moved: Vec<String> = r.moved.iter().filter(owned).cloned().collect();
        let unmoved: Vec<String> = r
            .deferred
            .iter()
            .chain(&r.stuck)
            .chain(unstacked)
            .filter(owned)
            .cloned()
            .collect();
        let steps = graphite::plan(
            self.cfg.pr.tool,
            &moved,
            &unmoved,
            |id| self.git.stack_parent(id),
            |id| {
                published == Some(id)
                    || self.awaiting_pr(id)
                    || self.released(id) && self.bead(id).is_some_and(Bead::has_open_pr)
            },
            |id| self.git.dirty(id).is_ok_and(|s| s.trim().is_empty()),
        );
        for step in steps {
            let (id, res) = match step {
                graphite::Step::Track { id, parent } => {
                    let parent = parent.map_or_else(|| self.base.clone(), |p| self.git.branch(&p));
                    let res = graphite::track(&self.repo, &self.git.branch(&id), &parent);
                    (id, res)
                }
                graphite::Step::Submit(id) => {
                    let res = graphite::submit(&self.git.worktree_path(&id), self.cfg.pr.draft);
                    if res.is_ok() {
                        r.resubmitted.push(id.clone());
                    }
                    (id, res)
                }
            };
            if let Err(e) = res {
                tracing::warn!("graphite sync of {id} failed: {e:#}");
                if !r.gt_failed.contains(&id) {
                    r.gt_failed.push(id);
                }
            }
        }
    }

    pub(super) fn reclaim_stack(&mut self, id: &str) -> Restacked {
        let closed = self
            .git
            .stack_parent(id)
            .filter(|p| self.bead(p).is_some_and(|b| b.status == "closed"));
        let mut restacked = None;
        if let Some(p) = closed {
            if self.git.unmerged_branch(&p, &self.base) {
                tracing::info!("{p} closed without merging, unstacking {id}");
                self.git.unstack(id);
            } else {
                restacked = Some(self.git.restack(id, &self.base));
            }
        }
        let mut r = Restacked::default();
        self.sync_graphite(&mut r, &[id.to_string()], None);
        match restacked {
            Some(Ok(())) => r.moved.push(id.to_string()),
            Some(Err(e)) => {
                tracing::warn!("restack of {id} failed: {e:#}");
                let fp = self.git.fingerprint(id);
                self.restack_stuck.insert(id.to_string(), fp);
                r.stuck.push(id.to_string());
            }
            None => {}
        }
        r
    }

    pub(super) fn sweep_stacks(&mut self) {
        let stacks = self.git.stacks();
        self.restack_stuck
            .retain(|id, _| stacks.iter().any(|(c, _)| c == id));
        let mut seeds = Vec::new();
        let mut unstacked = Vec::new();
        for (child, parent) in stacks {
            if self.released(&child) || self.bead(&parent).is_none_or(|b| b.status != "closed") {
                continue;
            }
            if self.git.unmerged_branch(&parent, &self.base) {
                tracing::info!("{parent} closed without merging, unstacking {child}");
                self.git.unstack(&child);
                unstacked.push(child);
                continue;
            }
            seeds.push(child);
        }
        let mut r = if seeds.is_empty() {
            Restacked::default()
        } else {
            self.restack_from(seeds)
        };
        let mut u = Restacked::default();
        self.sync_graphite(&mut u, &unstacked, None);
        r.gt_failed.extend(u.gt_failed);
        if !r.moved.is_empty() || !r.stuck.is_empty() || !r.gt_failed.is_empty() {
            self.status = r.summary().trim_start_matches(", ").to_string();
        }
    }

    fn close_merged(&self, id: &str) -> Result<()> {
        self.close_with(id, "merged by sulla")
    }

    pub(super) fn close_with(&self, id: &str, reason: &str) -> Result<()> {
        self.beads.close(id, reason).or_else(|e| {
            tracing::warn!("bd close {id} failed, retrying with --force: {e:#}");
            self.beads.force_close(id, reason)
        })?;
        if let Err(e) = sessions::remove(&sessions::backup_dir(&self.repo, id)) {
            tracing::warn!("removing session backup of {id}: {e:#}");
        }
        Ok(())
    }

    pub(super) fn resolve_conflicts(&mut self, id: &str) {
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
}
