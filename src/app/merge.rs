use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use color_eyre::Result;

use crate::agent;
use crate::beads::{Bead, Beads, State};
use crate::config::{PrTool, Workflow};
use crate::git::{Git, RebaseConflict};
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

pub(super) struct Stacker {
    git: Git,
    repo: PathBuf,
    base: String,
    tool: PrTool,
    graphite: bool,
    draft: bool,
    closed: HashSet<String>,
    released: HashSet<String>,
    awaiting: HashSet<String>,
    live: HashSet<String>,
    stuck: HashMap<String, Option<String>>,
    touched: HashSet<String>,
}

impl Stacker {
    fn released(&self, id: &str) -> bool {
        self.released.contains(id)
    }

    fn live(&self, id: &str) -> bool {
        self.live.contains(id) || self.live.contains(&agent::review_id(id))
    }

    fn stick(&mut self, id: &str, fp: Option<String>) {
        self.stuck.insert(id.to_string(), fp);
        self.touched.insert(id.to_string());
    }

    fn unstick(&mut self, id: &str) {
        if self.stuck.remove(id).is_some() {
            self.touched.insert(id.to_string());
        }
    }

    fn untrack(&self, id: &str) {
        untrack(self.graphite, &self.repo, &self.git.branch(id));
    }

    fn restack(
        &mut self,
        mut queue: Vec<String>,
        onto: &str,
        published: Option<&str>,
    ) -> Restacked {
        let mut r = Restacked::default();
        while let Some(c) = queue.pop() {
            if self.released(&c) {
                continue;
            }
            let fp = self.git.fingerprint(&c);
            if self.stuck.get(&c) == Some(&fp) {
                continue;
            }
            self.unstick(&c);
            if self.live(&c) {
                r.deferred.push(c);
                continue;
            }
            match self.git.restack(&c, onto) {
                Ok(()) => {
                    queue.extend(self.git.children(&c));
                    r.moved.push(c);
                }
                Err(e) => {
                    tracing::warn!("restack of {c} failed: {e:#}");
                    self.stick(&c, fp);
                    r.stuck.push(c);
                }
            }
        }
        self.sync_graphite(&mut r, &[], published);
        r
    }

    fn sync_graphite(&self, r: &mut Restacked, unstacked: &[String], published: Option<&str>) {
        if !self.graphite {
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
            self.tool,
            &moved,
            &unmoved,
            |id| self.git.stack_parent(id),
            |id| published == Some(id) || self.awaiting.contains(id),
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
                    let res = graphite::submit(&self.git.worktree_path(&id), self.draft);
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

    fn reclaim(&mut self, id: &str) -> Restacked {
        let closed = self
            .git
            .stack_parent(id)
            .filter(|p| self.closed.contains(p));
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
                self.stick(id, fp);
                r.stuck.push(id.to_string());
            }
            None => {}
        }
        r
    }

    fn sweep(&mut self, pairs: Vec<(String, String)>) -> Restacked {
        let mut seeds = Vec::new();
        let mut unstacked = Vec::new();
        for (child, parent) in pairs {
            if self.git.stack_parent(&child).as_deref() != Some(parent.as_str()) {
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
            let base = self.base.clone();
            self.restack(seeds, &base, None)
        };
        let mut u = Restacked::default();
        self.sync_graphite(&mut u, &unstacked, None);
        r.gt_failed.extend(u.gt_failed);
        r
    }
}

struct Closed {
    closed: Result<()>,
    removed: Result<()>,
    restacked: Restacked,
    stacker: Stacker,
}

fn untrack(graphite: bool, repo: &Path, branch: &str) {
    if !graphite {
        return;
    }
    if let Err(e) = graphite::untrack(repo, branch) {
        tracing::debug!("gt untrack {branch}: {e:#}");
    }
}

fn close_bead(beads: &Beads, repo: &Path, id: &str, reason: &str) -> Result<()> {
    beads.close(id, reason).or_else(|e| {
        tracing::warn!("bd close {id} failed, retrying with --force: {e:#}");
        beads.force_close(id, reason)
    })?;
    if let Err(e) = sessions::remove(&sessions::backup_dir(repo, id)) {
        tracing::warn!("removing session backup of {id}: {e:#}");
    }
    Ok(())
}

impl App {
    pub(super) fn request_merge(&mut self, id: String, force: bool) {
        if self.refuse_released(&id) || self.busy_merging(&id) {
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

    pub(super) fn busy_merging(&mut self, id: &str) -> bool {
        let busy = self.merging.contains(id);
        if busy {
            self.status = format!("{id} is already merging, wait for it to finish");
        }
        busy
    }

    pub(super) fn merge(&mut self, id: &str) {
        if self.busy_merging(id) {
            return;
        }
        self.merging.insert(id.to_string());
        self.check_merge(id.to_string());
    }

    fn check_merge(&mut self, id: String) {
        let key = id.clone();
        if self.stop_launch_then(&id, "merge it", move |app| app.check_merge(key)) {
            return;
        }
        self.status = format!("merging {id}...");
        let (beads, git, base, key) = (
            self.beads.clone(),
            self.git.clone(),
            self.base.clone(),
            id.clone(),
        );
        self.git_job(
            move || {
                let blockers = beads.open_blockers(&key);
                let merged =
                    matches!(&blockers, Ok(b) if b.is_empty()) && git.was_merged(&key, &base);
                (blockers, merged)
            },
            move |app, (blockers, merged)| app.merge_checked(id, blockers, merged),
        );
    }

    fn merge_checked(&mut self, id: String, blockers: Result<Vec<String>>, merged: bool) {
        let Some(blockers) = self.report(blockers) else {
            self.merging.remove(&id);
            return self.mutated();
        };
        if !blockers.is_empty() {
            self.merging.remove(&id);
            self.status = format!("cannot merge {id}: blocked by open {}", blockers.join(", "));
            return;
        }
        if merged {
            self.remove_agent(&id, Outcome::Merged);
            return self.close_merged(id, true);
        }
        self.land_merge(&id);
    }

    pub(super) fn land_merge(&mut self, id: &str) {
        self.merging.insert(id.to_string());
        self.status = format!("merging {id}...");
        let title = self.bead(id).map(|b| b.title.clone()).unwrap_or_default();
        self.stop_check(id);
        let slot = self.take_agent(id);
        self.finish(slot, id, Outcome::Merged);
        let msg = format!("{id}: {title}");
        let rejection = self.review_of(id).map(Diff::rejection).unwrap_or_default();
        let (git, base, key) = (self.git.clone(), self.base.clone(), id.to_string());
        let id = id.to_string();
        self.git_job(
            move || {
                git.drop_rejected(&key, &base, &msg, &rejection)
                    .and_then(|_| git.land(&key, &base, &msg))
            },
            move |app, r| app.landed(id, r),
        );
    }

    fn landed(&mut self, id: String, r: Result<()>) {
        if let Err(e) = &r
            && e.downcast_ref::<RebaseConflict>().is_some()
        {
            self.merging.remove(&id);
            self.status = format!("merge conflicts in {id}");
            if matches!(self.mode, Mode::Normal) {
                self.mode = Mode::Confirm {
                    msg: format!(
                        "Merge of {id} hit conflicts with {}. Have the agent resolve them automatically? y/n",
                        self.base
                    ),
                    action: Action::ResolveConflicts(id),
                };
            } else {
                self.status.push_str(", m merges again to resolve them");
            }
            return self.mutated();
        }
        if self.report(r).is_none() {
            self.merging.remove(&id);
            return self.mutated();
        }
        self.close_merged(id, false);
    }

    fn close_merged(&mut self, id: String, already: bool) {
        let beads = self.beads.clone();
        let mut s = self.stacker();
        let key = id.clone();
        self.git_job(
            move || {
                let closed = close_bead(&beads, &s.repo, &key, "merged by sulla");
                s.untrack(&key);
                let removed = s.git.remove(&key);
                let children = s.git.children(&key);
                let base = s.base.clone();
                let restacked = s.restack(children, &base, None);
                Closed {
                    closed,
                    removed,
                    restacked,
                    stacker: s,
                }
            },
            move |app, c| app.merged(&id, already, c),
        );
    }

    fn merged(&mut self, id: &str, already: bool, c: Closed) {
        self.merging.remove(id);
        self.status = match (already, c.closed) {
            (true, Ok(())) => format!("{id} was already merged, closed bead"),
            (true, Err(e)) => format!("{id} was already merged but bd close failed: {e}"),
            (false, Ok(())) => format!("merged {id} into {}", self.base),
            (false, Err(e)) => format!("merged {id} into {} but bd close failed: {e}", self.base),
        };
        if let Err(e) = c.removed {
            tracing::warn!("cleaning up {id} after merge: {e:#}");
            self.status.push_str(&format!(
                "; cleanup failed: {e}. Remove {} by hand, then run `git worktree prune` and `git branch -D {}`",
                self.git.worktree_path(id).display(),
                self.git.branch(id)
            ));
        }
        self.status.push_str(&c.restacked.summary());
        self.restacked(c.stacker);
        if self.viewing().as_deref() == Some(id) {
            if matches!(self.mode, Mode::Insert) {
                self.mode = Mode::Normal;
            }
            self.view = View::Board;
        }
        self.mutated();
    }

    pub(super) fn stacker(&self) -> Stacker {
        let ids = |keep: &dyn Fn(&Bead) -> bool| -> HashSet<String> {
            self.all
                .iter()
                .filter(|b| keep(b))
                .map(|b| b.id.clone())
                .collect()
        };
        let open = |b: &Bead| b.status != "closed";
        let released = |b: &Bead| open(b) && b.state() == Some(State::Released);
        let live = self
            .agents
            .iter()
            .filter(|(_, a)| a.alive)
            .map(|(k, _)| k)
            .chain(self.launching.keys())
            .chain(&self.merging)
            .cloned()
            .collect();
        Stacker {
            git: self.git.clone(),
            repo: self.repo.clone(),
            base: self.base.clone(),
            tool: self.cfg.pr.tool,
            graphite: self.graphite(),
            draft: self.cfg.pr.draft,
            closed: ids(&|b| !open(b)),
            released: ids(&released),
            awaiting: ids(&|b| {
                open(b) && b.state() == Some(State::Pr) || released(b) && b.has_open_pr()
            }),
            live,
            stuck: self.restack_stuck.clone(),
            touched: HashSet::new(),
        }
    }

    fn restacked(&mut self, s: Stacker) {
        for id in s.touched {
            match s.stuck.get(&id) {
                Some(fp) => self.restack_stuck.insert(id, fp.clone()),
                None => self.restack_stuck.remove(&id),
            };
        }
    }

    pub(super) fn restack_onto(&mut self, queue: Vec<String>, onto: String) -> Restacked {
        self.restack_synced(queue, onto, None)
    }

    pub(super) fn restack_synced(
        &mut self,
        queue: Vec<String>,
        onto: String,
        published: Option<&str>,
    ) -> Restacked {
        let mut s = self.stacker();
        let r = s.restack(queue, &onto, published);
        self.restacked(s);
        r
    }

    pub(super) fn graphite(&self) -> bool {
        self.cfg.workflow == Workflow::Pr && self.cfg.pr.tool == PrTool::Graphite
    }

    pub(super) fn untrack(&self, id: &str) {
        untrack(self.graphite(), &self.repo, &self.git.branch(id));
    }

    pub(super) fn sync_graphite(
        &self,
        r: &mut Restacked,
        unstacked: &[String],
        published: Option<&str>,
    ) {
        self.stacker().sync_graphite(r, unstacked, published);
    }

    pub(super) fn reclaim_stack(&mut self, id: &str) -> Restacked {
        let mut s = self.stacker();
        let r = s.reclaim(id);
        self.restacked(s);
        r
    }

    pub(super) fn sweep_stacks(&mut self) {
        if self.sweeping {
            return;
        }
        let stacks = self.git.stacks();
        self.restack_stuck
            .retain(|id, _| stacks.iter().any(|(c, _)| c == id));
        let pairs: Vec<(String, String)> = stacks
            .into_iter()
            .filter(|(child, parent)| {
                !self.released(child) && self.bead(parent).is_some_and(|b| b.status == "closed")
            })
            .collect();
        if pairs.is_empty() {
            return;
        }
        self.sweeping = true;
        let mut s = self.stacker();
        self.git_job(
            move || {
                let r = s.sweep(pairs);
                (r, s)
            },
            |app, (r, s)| {
                app.sweeping = false;
                app.restacked(s);
                if !r.moved.is_empty() || !r.stuck.is_empty() || !r.gt_failed.is_empty() {
                    app.status = r.summary().trim_start_matches(", ").to_string();
                }
            },
        );
    }

    pub(super) fn close_with(&self, id: &str, reason: &str) -> Result<()> {
        close_bead(&self.beads, &self.repo, id, reason)
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
