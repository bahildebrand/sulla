use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use crate::agent;
use crate::app::Event;
use crate::beads::{Bead, Beads, State};

pub struct Snapshot {
    pub taken: Instant,
    pub all: Vec<Bead>,
    pub ready: Vec<Bead>,
    pub blocked: HashMap<String, Vec<String>>,
}

pub fn spawn_poller(beads: Beads, every: Duration, tx: Sender<Event>) -> Sender<()> {
    let (refresh_tx, refresh_rx): (Sender<()>, Receiver<()>) = channel();
    std::thread::spawn(move || {
        loop {
            let taken = Instant::now();
            let snap = beads
                .list_all()
                .and_then(|all| {
                    Ok(Snapshot {
                        taken,
                        all,
                        ready: beads.ready()?,
                        blocked: beads.blocked()?,
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
            Some(State::Running) => !known.contains(&b.id),
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
    fn reviewing_bead_is_orphaned_only_without_reviewer() {
        let all = vec![
            bead("run", 1, "2026-01-01T00:00:00Z", &["sulla:running"]),
            bead("rev", 1, "2026-01-01T00:00:00Z", &["sulla:reviewing"]),
            bead("live", 1, "2026-01-01T00:00:00Z", &["sulla:reviewing"]),
            bead("done", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
        ];
        let known = HashSet::from(["rev".to_string(), agent::review_id("live")]);
        let ids: Vec<_> = orphans(&all, &known)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["run", "rev"]);
    }
}
