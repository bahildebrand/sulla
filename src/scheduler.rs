use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use crate::agent;
use crate::app::Event;
use crate::beads::{Bead, Beads, State};
use crate::conflicts::{Conflict, Scanner};
use crate::git::{DiffStat, Git};

pub struct Snapshot {
    pub taken: Instant,
    pub all: Vec<Bead>,
    pub ready: Vec<Bead>,
    pub blocked: HashMap<String, Vec<String>>,
    pub diffstats: HashMap<String, DiffStat>,
    pub conflicts: HashMap<String, Conflict>,
}

fn diffstats(git: &Git, base: &str, all: &[Bead]) -> HashMap<String, DiffStat> {
    all.iter()
        .filter(|b| b.status != "closed")
        .filter_map(|b| match git.diff_stat(&b.id, base) {
            Ok(s) => Some((b.id.clone(), s?)),
            Err(e) => {
                tracing::debug!("diff stat {}: {e:#}", b.id);
                None
            }
        })
        .collect()
}

pub fn spawn_poller(
    beads: Beads,
    git: Git,
    base: String,
    every: Duration,
    tx: Sender<Event>,
) -> Sender<()> {
    let (refresh_tx, refresh_rx): (Sender<()>, Receiver<()>) = channel();
    std::thread::spawn(move || {
        let mut scanner = Scanner::default();
        loop {
            let taken = Instant::now();
            let snap = beads
                .list_all()
                .and_then(|all| {
                    let open: HashSet<String> = all
                        .iter()
                        .filter(|b| b.status != "closed")
                        .map(|b| b.id.clone())
                        .collect();
                    let conflicts = scanner.scan(&git, &base, &open).unwrap_or_else(|e| {
                        tracing::warn!("conflict scan: {e:#}");
                        HashMap::new()
                    });
                    Ok(Snapshot {
                        taken,
                        diffstats: diffstats(&git, &base, &all),
                        all,
                        ready: beads.ready()?,
                        blocked: beads.blocked()?,
                        conflicts,
                    })
                })
                .map_err(|e| e.to_string());
            if tx.send(Event::Snapshot(snap)).is_err() {
                return;
            }
            match refresh_rx.recv_timeout(every) {
                Ok(()) => while refresh_rx.try_recv().is_ok() {},
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    refresh_tx
}

pub const BACKOFF_BASE: Duration = Duration::from_secs(60);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct Backoff {
    until: Option<Instant>,
    delay: Duration,
    last_hit: Option<Instant>,
}

impl Backoff {
    pub fn hit(&mut self, now: Instant) -> Option<Duration> {
        if self.remaining(now).is_some() {
            return None;
        }
        self.delay = match self.last_hit {
            Some(t) if now.duration_since(t) < self.delay * 2 => (self.delay * 2).min(BACKOFF_MAX),
            _ => BACKOFF_BASE,
        };
        self.last_hit = Some(now);
        self.until = Some(now + self.delay);
        Some(self.delay)
    }

    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until
            .and_then(|u| u.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    pub fn expired(&mut self, now: Instant) -> bool {
        let done = self.until.is_some() && self.remaining(now).is_none();
        if done {
            self.until = None;
        }
        done
    }

    pub fn resume(&mut self) -> bool {
        self.until.take().is_some()
    }
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs_f64().ceil() as u64;
    match s {
        0..60 => format!("{s}s"),
        _ => format!("{}m{:02}s", s / 60, s % 60),
    }
}

pub fn pick<'a>(
    ready: &'a [Bead],
    busy: &HashSet<String>,
    skip: &HashSet<String>,
    free: usize,
) -> Vec<&'a Bead> {
    let mut candidates: Vec<&Bead> = ready
        .iter()
        .filter(|b| !matches!(b.state(), Some(State::Held | State::Backlog)))
        .filter(|b| !busy.contains(&b.id) && !skip.contains(&b.id))
        .collect();
    candidates.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(a.created_at.cmp(&b.created_at))
    });
    candidates.truncate(free);
    candidates
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Idle {
    Paused,
    RateLimited(Duration),
    SlotsFull {
        running: usize,
        max: usize,
    },
    Failed(Vec<String>),
    Blocked(usize),
    Backlog(usize),
    Waiting {
        blocked: usize,
        backlog: usize,
        held: usize,
    },
    Empty,
}

impl Idle {
    pub fn describe(&self) -> String {
        match self {
            Idle::Paused => "scheduling paused (P or :resume)".into(),
            Idle::RateLimited(d) => format!(
                "rate limited, resuming in {} (:resume to override)",
                fmt_duration(*d)
            ),
            Idle::SlotsFull { running, max } if running < max => {
                format!("all {max} slots in use ({running} working agents, :max <n> to raise)")
            }
            Idle::SlotsFull { running, max } => {
                format!("slots full, {running}/{max} agents running (:max <n> to raise)")
            }
            Idle::Failed(ids) => {
                format!("failed to start {} (:refresh to retry)", ids.join(", "))
            }
            Idle::Blocked(1) => "the only pending bead is blocked".into(),
            Idle::Blocked(n) => format!("all {n} pending beads are blocked"),
            Idle::Backlog(n) => format!(
                "backlog only, {n} bead{} in backlog (s or S to release)",
                if *n == 1 { "" } else { "s" }
            ),
            Idle::Waiting {
                blocked,
                backlog,
                held,
            } => {
                let parts: Vec<String> =
                    [(blocked, "blocked"), (backlog, "backlog"), (held, "held")]
                        .into_iter()
                        .filter(|(n, _)| **n > 0)
                        .map(|(n, s)| format!("{n} {s}"))
                        .collect();
                format!("nothing ready, {}", parts.join(", "))
            }
            Idle::Empty => "no open beads to start".into(),
        }
    }
}

pub fn why_idle(
    all: &[Bead],
    blocked: &HashMap<String, Vec<String>>,
    failed: &HashSet<String>,
) -> Idle {
    let mut failed_ids = Vec::new();
    let (mut n_blocked, mut backlog, mut held) = (0, 0, 0);
    for b in all
        .iter()
        .filter(|b| b.status != "closed" && b.issue_type != "epic")
    {
        if failed.contains(&b.id) {
            failed_ids.push(b.id.clone());
            continue;
        }
        match b.state() {
            Some(State::Backlog) => backlog += 1,
            Some(State::Held) => held += 1,
            None if blocked.contains_key(&b.id) => n_blocked += 1,
            _ => {}
        }
    }
    if !failed_ids.is_empty() {
        failed_ids.sort();
        return Idle::Failed(failed_ids);
    }
    match (n_blocked, backlog, held) {
        (0, 0, 0) => Idle::Empty,
        (n, 0, 0) => Idle::Blocked(n),
        (0, n, 0) => Idle::Backlog(n),
        (blocked, backlog, held) => Idle::Waiting {
            blocked,
            backlog,
            held,
        },
    }
}

pub fn stack_parent<'a>(
    id: &str,
    blocked: &'a HashMap<String, Vec<String>>,
    all: &[Bead],
) -> Option<&'a str> {
    let [parent] = blocked.get(id)?.as_slice() else {
        return None;
    };
    all.iter()
        .find(|b| b.id == *parent)
        .filter(|b| {
            b.status != "closed" && matches!(b.state(), Some(State::Review | State::Reviewing))
        })
        .map(|_| parent.as_str())
}

pub fn stackable<'a>(all: &'a [Bead], blocked: &HashMap<String, Vec<String>>) -> Vec<&'a Bead> {
    all.iter()
        .filter(|b| b.status == "open" && b.state().is_none() && b.issue_type != "epic")
        .filter(|b| stack_parent(&b.id, blocked, all).is_some())
        .collect()
}

pub fn orphans<'a>(all: &'a [Bead], known: &HashSet<String>) -> Vec<&'a Bead> {
    all.iter()
        .filter(|b| b.status != "closed")
        .filter(|b| match b.state() {
            Some(State::Running | State::Review) => !known.contains(&b.id),
            Some(State::Reviewing) => !known.contains(&agent::review_id(&b.id)),
            _ => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(id: &str, pri: u8, created: &str, labels: &[&str]) -> Bead {
        Bead {
            id: id.into(),
            title: id.into(),
            status: "open".into(),
            priority: pri,
            issue_type: "task".into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            description: String::new(),
            notes: String::new(),
            created_at: Some(created.parse().unwrap()),
            closed_at: None,
            updated_at: None,
            parent: None,
            state_changed_at: None,
            dependencies: Vec::new(),
        }
    }

    #[test]
    fn picks_by_priority_then_age_within_capacity() {
        let ready = vec![
            bead("low", 3, "2026-01-01T00:00:00Z", &[]),
            bead("new-p1", 1, "2026-01-03T00:00:00Z", &[]),
            bead("old-p1", 1, "2026-01-02T00:00:00Z", &[]),
            bead("held", 0, "2026-01-01T00:00:00Z", &["sulla:held"]),
            bead("busy", 0, "2026-01-01T00:00:00Z", &[]),
            bead("backlog", 0, "2026-01-01T00:00:00Z", &["sulla:backlog"]),
        ];
        let busy = HashSet::from(["busy".to_string()]);
        let ids: Vec<_> = pick(&ready, &busy, &HashSet::new(), 2)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["old-p1", "new-p1"]);
        assert!(pick(&ready, &busy, &HashSet::new(), 0).is_empty());
    }

    #[test]
    fn backoff_doubles_on_quick_repeat_and_resets_later() {
        let t0 = Instant::now();
        let mut b = Backoff::default();
        assert_eq!(b.hit(t0), Some(BACKOFF_BASE));
        assert_eq!(b.hit(t0 + Duration::from_secs(5)), None);
        assert!(b.remaining(t0 + Duration::from_secs(30)).is_some());
        assert!(!b.expired(t0 + Duration::from_secs(30)));
        let t1 = t0 + BACKOFF_BASE;
        assert!(b.remaining(t1).is_none());
        assert!(b.expired(t1));
        assert!(!b.expired(t1));
        assert_eq!(b.hit(t1 + Duration::from_secs(10)), Some(BACKOFF_BASE * 2));
        let mut t = t1 + Duration::from_secs(10);
        for _ in 0..10 {
            t += b.remaining(t).unwrap();
            b.hit(t);
        }
        assert_eq!(b.remaining(t), Some(BACKOFF_MAX));
        assert!(b.resume());
        assert!(!b.resume());
        assert_eq!(b.hit(t + BACKOFF_MAX * 3), Some(BACKOFF_BASE));
    }

    #[test]
    fn idle_reason_classifies_pending_work() {
        let none = HashSet::new();
        let blocked = HashMap::from([
            ("b1".to_string(), vec!["x".to_string()]),
            ("b2".to_string(), vec!["x".to_string()]),
        ]);
        let mut epic = bead("e", 1, "2026-01-01T00:00:00Z", &[]);
        epic.issue_type = "epic".into();
        let mut done = bead("d", 1, "2026-01-01T00:00:00Z", &["sulla:backlog"]);
        done.status = "closed".into();
        let b1 = bead("b1", 1, "2026-01-01T00:00:00Z", &[]);
        let b2 = bead("b2", 1, "2026-01-01T00:00:00Z", &[]);
        let bl = bead("bl", 1, "2026-01-01T00:00:00Z", &["sulla:backlog"]);
        let held = bead("h", 1, "2026-01-01T00:00:00Z", &["sulla:held"]);
        let run = bead("r", 1, "2026-01-01T00:00:00Z", &["sulla:running"]);

        let base = vec![epic, done, run];
        let with = |extra: &[&Bead]| {
            let mut v = base.clone();
            v.extend(extra.iter().map(|b| (*b).clone()));
            v
        };
        assert_eq!(why_idle(&base, &blocked, &none), Idle::Empty);
        assert_eq!(
            why_idle(&with(&[&b1, &b2]), &blocked, &none),
            Idle::Blocked(2)
        );
        assert_eq!(why_idle(&with(&[&bl]), &blocked, &none), Idle::Backlog(1));
        assert_eq!(
            why_idle(&with(&[&b1, &bl, &held]), &blocked, &none),
            Idle::Waiting {
                blocked: 1,
                backlog: 1,
                held: 1
            }
        );
        assert_eq!(
            Idle::Waiting {
                blocked: 0,
                backlog: 0,
                held: 2
            }
            .describe(),
            "nothing ready, 2 held"
        );
        let failed = HashSet::from(["bl".to_string(), "gone".to_string()]);
        assert_eq!(
            why_idle(&with(&[&b1, &bl]), &blocked, &failed),
            Idle::Failed(vec!["bl".into()])
        );
    }

    #[test]
    fn stacks_only_on_single_blocker_in_review() {
        let all = vec![
            bead("rev", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
            bead("run", 1, "2026-01-01T00:00:00Z", &["sulla:running"]),
            bead("on-rev", 1, "2026-01-01T00:00:00Z", &[]),
            bead("on-run", 1, "2026-01-01T00:00:00Z", &[]),
            bead("on-both", 1, "2026-01-01T00:00:00Z", &[]),
            bead("held", 1, "2026-01-01T00:00:00Z", &["sulla:held"]),
        ];
        let blocked = HashMap::from([
            ("on-rev".to_string(), vec!["rev".to_string()]),
            ("on-run".to_string(), vec!["run".to_string()]),
            (
                "on-both".to_string(),
                vec!["rev".to_string(), "run".to_string()],
            ),
            ("held".to_string(), vec!["rev".to_string()]),
        ]);
        assert_eq!(stack_parent("on-rev", &blocked, &all), Some("rev"));
        assert_eq!(stack_parent("on-run", &blocked, &all), None);
        assert_eq!(stack_parent("on-both", &blocked, &all), None);
        let ids: Vec<_> = stackable(&all, &blocked)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["on-rev"]);
    }

    #[test]
    fn reviewing_bead_is_orphaned_only_without_reviewer() {
        let all = vec![
            bead("run", 1, "2026-01-01T00:00:00Z", &["sulla:running"]),
            bead("rev", 1, "2026-01-01T00:00:00Z", &["sulla:reviewing"]),
            bead("live", 1, "2026-01-01T00:00:00Z", &["sulla:reviewing"]),
            bead("done", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
            bead("idle", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
        ];
        let known = HashSet::from([
            "rev".to_string(),
            "done".to_string(),
            agent::review_id("live"),
        ]);
        let ids: Vec<_> = orphans(&all, &known)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["run", "rev", "idle"]);
    }
}
