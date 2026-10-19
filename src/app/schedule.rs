use std::collections::HashSet;
use std::time::Instant;

use color_eyre::Result;

use crate::agent::{self, CHAT_ID};
use crate::beads::{Bead, State};
use crate::checks;
use crate::config::Workflow;
use crate::sandbox::Outcome;
use crate::scheduler::{self, Idle};

use super::{App, Mode, View};

impl App {
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

    pub(super) fn reconcile(&mut self) {
        self.reap_reviewers();
        self.sweep_stacks();
        let known: HashSet<String> = self.busy().cloned().collect();
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
                let id = b.id.clone();
                self.bd_run(move |beads| beads.set_state(&id, State::Review));
                self.edit_local(&b.id, |b| b.set_state_label(Some(State::Review)));
                if known.contains(&b.id) {
                    continue;
                }
            }
            let has_worktree = self.git.worktree_path(&b.id).join(".git").exists();
            let in_review = matches!(state, Some(State::Review | State::Reviewing));
            if in_review && (!has_worktree || !self.spare_slot()) {
                continue;
            }
            if has_worktree {
                if self.backoff.remaining(Instant::now()).is_some()
                    || (!in_review && !self.make_room(&b.id))
                {
                    continue;
                }
                tracing::info!("resuming orphaned agent {}", b.id);
                let id = b.id.clone();
                self.start_agent(b, true, None, move |app, r| {
                    if app.report(r).is_none() {
                        app.failed.insert(id);
                    }
                    app.rebuilt_status();
                });
            } else {
                tracing::info!("resetting orphaned bead {}", b.id);
                self.edit_local(&b.id, |b| {
                    b.set_state_label(None);
                    b.status = "open".into();
                });
                self.bd_run(move |beads| {
                    beads.clear_state(&b)?;
                    beads.unclaim(&b.id)
                });
            }
        }
    }

    pub(super) fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if paused {
            self.status = "scheduling paused".into();
        } else {
            self.backoff.resume();
            self.status = "scheduling resumed".into();
            self.schedule();
        }
    }

    pub(super) fn free_slots(&self) -> usize {
        self.reach(0)
    }

    fn reach(&self, evictable: usize) -> usize {
        self.cfg
            .max_agents
            .saturating_sub(self.running_count() + self.launching.len())
            .min(self.slots.free(self.cfg.max_agents) + evictable)
            .saturating_sub(self.starting.len())
    }

    fn spare_slot(&self) -> bool {
        let spare = self
            .slots
            .free(self.cfg.max_agents)
            .saturating_sub(self.starting.len());
        let wanted = if self.paused {
            0
        } else {
            self.picks(self.free_slots()).len()
        };
        spare > wanted
    }

    fn is_parked(&self, id: &str) -> bool {
        let rid = agent::review_id(id);
        id != CHAT_ID
            && agent::reviewed_bead(id).is_none()
            && self
                .bead(id)
                .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Review))
            && self
                .slots
                .get(id)
                .is_some_and(|s| self.slots.holders(s) == 1)
            && !self.agents.get(&rid).is_some_and(|a| a.alive)
            && !self.is_starting(&rid)
            && !self.is_starting(id)
            && !self.checks.get(id).is_some_and(|c| {
                matches!(c.status, checks::Status::Queued | checks::Status::Running)
            })
            && !self.publishing.contains(id)
            && !self.republishing.contains_key(id)
            && !self.republishing.values().any(|top| top == id)
            && !(matches!(self.mode, Mode::Insert)
                && matches!(&self.view, View::Progress(v) if v == id))
    }

    fn parked(&self) -> Vec<String> {
        let mut parked: Vec<&Bead> = self
            .agents
            .keys()
            .filter(|id| self.is_parked(id))
            .filter_map(|id| self.bead(id))
            .collect();
        parked.sort_by_key(|b| (b.state_since(), b.id.clone()));
        parked.into_iter().map(|b| b.id.clone()).collect()
    }

    fn evict(&mut self, victim: &str, wanted_by: &str) {
        tracing::info!("evicting parked agent {victim} to make room for {wanted_by}");
        self.remove_agent(victim, Outcome::Evicted);
        self.status = format!("freed slot from parked {victim} for {wanted_by}");
    }

    pub(super) fn make_room(&mut self, id: &str) -> bool {
        let starting = self.starting.iter().filter(|s| *s != id).count();
        if self.slots.get(id).is_some()
            || self.slots.get(&agent::review_id(id)).is_some()
            || self.slots.free(self.cfg.max_agents) > starting
        {
            return true;
        }
        let Some(victim) = self.parked().into_iter().find(|v| v != id) else {
            return false;
        };
        self.evict(&victim, id);
        true
    }

    pub(super) fn no_room(&mut self, id: &str) {
        self.status = format!(
            "no free agent slot for {id}: all {} hold working agents, none parked in review",
            self.cfg.max_agents
        );
    }

    fn evict_for_ready(&mut self) {
        let parked = self.parked();
        let free = self.free_slots();
        let reach = self.reach(parked.len());
        if reach <= free {
            return;
        }
        let picks = self.picks(reach);
        for (victim, b) in parked.iter().zip(picks.iter().skip(free)) {
            self.evict(victim, &b.id);
        }
    }

    pub(super) fn busy(&self) -> impl Iterator<Item = &String> {
        self.agents
            .keys()
            .chain(&self.starting)
            .chain(self.launching.keys())
    }

    pub fn is_starting(&self, id: &str) -> bool {
        self.starting.contains(id) || self.launching.contains_key(id)
    }

    pub(super) fn picks(&self, free: usize) -> Vec<Bead> {
        let busy: HashSet<String> = self.busy().cloned().collect();
        let mut candidates = self.ready.clone();
        candidates.extend(
            scheduler::stackable(&self.all, &self.blocked, self.cfg.workflow == Workflow::Pr)
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
        let reach = self.reach(self.parked().len());
        if reach == 0 {
            return Some(Idle::SlotsFull {
                running: self.running_count(),
                max: self.cfg.max_agents,
            });
        }
        if !self.picks(reach).is_empty() {
            return None;
        }
        Some(scheduler::why_idle(&self.all, &self.blocked, &self.failed))
    }

    pub(super) fn schedule(&mut self) {
        if self.paused || self.backoff.remaining(Instant::now()).is_some() {
            return;
        }
        self.evict_for_ready();
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
            self.starting.insert(b.id.clone());
            let id = b.id.clone();
            self.bd(
                move |beads| {
                    beads.claim(&b.id)?;
                    beads.set_state(&b.id, State::Running)?;
                    Ok(beads.show(&b.id).unwrap_or(b))
                },
                move |app, r| app.claimed(id, r),
            );
        }
    }

    fn claimed(&mut self, id: String, r: Result<Bead>) {
        self.starting.remove(&id);
        if self.agents.contains_key(&id) || self.launching.contains_key(&id) {
            return;
        }
        match r {
            Ok(b) => self.launch_agent(&b, false, None, move |app, r| app.started(id, r)),
            Err(e) => self.started(id, Err(e)),
        }
    }

    pub(super) fn started(&mut self, id: String, r: Result<()>) {
        if self.report(r).is_some() {
            self.status = format!("started {id}");
        } else {
            self.failed.insert(id.clone());
            self.bd_run(move |beads| beads.unclaim(&id));
        }
    }

    pub(super) fn stack_on(&self, id: &str) -> Option<String> {
        let on_pr = self.cfg.workflow == Workflow::Pr;
        scheduler::stack_parent(id, &self.blocked, &self.all, on_pr)
            .filter(|p| self.git.has_branch(p))
            .map(String::from)
    }
}
