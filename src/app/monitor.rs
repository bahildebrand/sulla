use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::agent::{self, CHAT_ID, Stall};
use crate::beads::State;
use crate::budget::{self, Budget};
use crate::config::Trigger;
use crate::sandbox::Outcome;
use crate::scheduler::fmt_duration;
use crate::usage::Watch;

use super::{App, Mode, View};

const STALL_POLL: Duration = Duration::from_secs(1);
const RATE_SCAN_EVERY: Duration = Duration::from_secs(1);

impl App {
    pub(super) fn check_stalls(&mut self) {
        let now = Instant::now();
        if self
            .stall_checked
            .is_some_and(|t| now.duration_since(t) < STALL_POLL)
        {
            return;
        }
        self.stall_checked = Some(now);
        let mut next = HashMap::new();
        for (id, a) in &self.agents {
            let (bid, want) = match agent::reviewed_bead(id) {
                Some(b) => (b, State::Reviewing),
                None => (id.as_str(), State::Running),
            };
            let watched = self
                .bead(bid)
                .is_some_and(|b| b.status != "closed" && b.state() == Some(want));
            if let Some(s) = watched.then(|| a.stall(now, &self.cfg.stall)).flatten() {
                next.insert(id.clone(), s);
            }
        }
        for id in &self.failed {
            let orphaned = !self.agents.contains_key(id)
                && self
                    .bead(id)
                    .is_some_and(|b| b.status != "closed" && b.state() == Some(State::Running));
            if orphaned {
                next.insert(id.clone(), Stall::Dead);
            }
        }
        let prev = std::mem::replace(&mut self.stalls, next);
        for id in prev.keys().filter(|id| !self.stalls.contains_key(*id)) {
            let bid = agent::reviewed_bead(id).unwrap_or(id);
            self.notifier
                .retain(|i, t| i != bid || matches!(t, Trigger::Review | Trigger::Budget));
        }
        let fresh: Vec<(String, Stall)> = self
            .stalls
            .iter()
            .filter(|(id, s)| prev.get(*id) != Some(*s))
            .map(|(id, s)| (id.clone(), *s))
            .collect();
        for (id, s) in fresh {
            let bid = agent::reviewed_bead(&id).unwrap_or(&id);
            let Some((bid, title)) = self.bead(bid).map(|b| (b.id.clone(), b.title.clone())) else {
                continue;
            };
            let trigger = match s {
                Stall::Waiting => Trigger::Waiting,
                Stall::Stalled | Stall::Dead => Trigger::Stalled,
            };
            tracing::info!("agent {id} {s:?}");
            self.notify(trigger, &bid, &title);
        }
    }

    pub fn budget_status(&self, id: &str) -> Option<String> {
        let run = self.budget_runs.get(id)?;
        let budget = Budget::for_bead(&self.cfg.budget, self.bead(id)?);
        if budget.is_unlimited() {
            return None;
        }
        let used = run.used(self.usage.get(id).copied().unwrap_or_default());
        Some(budget.progress(run.elapsed(Instant::now()), used))
    }

    pub(super) fn check_budgets(&mut self) {
        let now = Instant::now();
        if self
            .budget_checked
            .is_some_and(|t| now.duration_since(t) < STALL_POLL)
        {
            return;
        }
        self.budget_checked = Some(now);
        let running: Vec<(String, Budget)> = self
            .all
            .iter()
            .filter(|b| b.status != "closed" && b.state() == Some(State::Running))
            .filter(|b| self.agents.get(&b.id).is_some_and(|a| a.alive))
            .map(|b| (b.id.clone(), Budget::for_bead(&self.cfg.budget, b)))
            .collect();
        self.budget_runs
            .retain(|id, _| running.iter().any(|(r, _)| r == id));
        let mut over = Vec::new();
        for (id, budget) in running {
            let current = self
                .usage_seen
                .then(|| self.usage.get(&id).copied().unwrap_or_default());
            let run = self
                .budget_runs
                .entry(id.clone())
                .or_insert_with(|| budget::Run::new(now, current));
            if let Some(c) = current {
                run.observe(c);
            }
            let used = run.used(current.unwrap_or_default());
            if let Some(msg) = budget.exceeded(run.elapsed(now), used) {
                over.push((id, msg));
            }
        }
        for (id, msg) in over {
            self.hold_over_budget(&id, &msg);
        }
    }

    fn hold_over_budget(&mut self, id: &str, msg: &str) {
        tracing::info!("{id}: {msg}");
        self.stop_check(id);
        self.remove_agent(id, Outcome::Killed);
        self.budget_runs.remove(id);
        let (bead, reason) = (id.to_string(), msg.to_string());
        self.bd_run(move |b| {
            b.set_state_because(&bead, State::Held, &reason)?;
            b.unclaim(&bead)
        });
        self.edit_local(id, |b| {
            b.set_state_label(Some(State::Held));
            b.status = "open".into();
        });
        self.status = format!("{id} {msg}, held in Pending (s to release)");
        self.over_budget.insert(id.to_string(), msg.to_string());
        if matches!(&self.view, View::Progress(v) if v == id) {
            if matches!(self.mode, Mode::Insert) {
                self.mode = Mode::Normal;
            }
            self.view = View::Board;
        }
        if let Some(title) = self.bead(id).map(|b| b.title.clone()) {
            self.notify(Trigger::Budget, id, &title);
        }
    }

    pub(super) fn entered_review(&mut self) -> Vec<String> {
        let now: HashMap<String, Option<State>> =
            self.all.iter().map(|b| (b.id.clone(), b.state())).collect();
        let Some(prev) = self.states.replace(now) else {
            return Vec::new();
        };
        self.all
            .iter()
            .filter(|b| b.status != "closed" && b.state() == Some(State::Review))
            .filter(|b| prev.get(&b.id).is_some_and(|s| *s != Some(State::Review)))
            .map(|b| b.id.clone())
            .collect()
    }

    pub(super) fn check_rate_limits(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.rate_scanned) < RATE_SCAN_EVERY {
            return;
        }
        self.rate_scanned = now;
        for (id, a) in &mut self.agents {
            if a.retry_rejected_prompt(now) {
                tracing::warn!("{id} rejected its prompt while loading policy, resubmitted");
            }
        }
        let mut seen = HashMap::new();
        let mut fresh = None;
        for (id, a) in &self.agents {
            if !a.alive {
                continue;
            }
            if let Some(hit) = a.rate_limit() {
                if self.rate_seen.get(id) != Some(&hit) && fresh.is_none() {
                    fresh = Some(id.clone());
                }
                seen.insert(id.clone(), hit);
            }
        }
        self.rate_seen = seen;
        if let Some(id) = fresh {
            if let Some(d) = self.backoff.hit(now) {
                tracing::warn!("rate limit from {id}, pausing scheduler for {d:?}");
                self.status = format!(
                    "rate limit from {id}, pausing scheduler for {} (:resume to override)",
                    fmt_duration(d)
                );
            }
        } else if self.backoff.expired(now) {
            self.status = "rate limit backoff over, scheduler resumed".into();
            self.mutated();
        }
    }

    pub(super) fn watch_usage(&self) {
        let now = Utc::now();
        let mut watch: Watch = self
            .all
            .iter()
            .filter(|b| b.column(now, self.cfg.done_retention_days).is_some())
            .map(|b| {
                let sessions = [b.id.clone(), agent::review_id(&b.id)]
                    .iter()
                    .map(|id| agent::session_id(&self.repo, id))
                    .collect();
                let kind = agent::selection(b, self.cfg.agent).0;
                (b.id.clone(), kind, sessions)
            })
            .collect();
        if !self.chat_sessions.is_empty() {
            watch.push((
                CHAT_ID.to_string(),
                self.cfg.agent,
                self.chat_sessions.clone(),
            ));
        }
        let _ = self.usage_watch.send(watch);
    }
}
