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
