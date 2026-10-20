use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use crate::agent;
use crate::app::Event;
use crate::beads::{Bead, Beads, State};
use crate::conflicts::{Conflict, Scanner};
use crate::git::{DiffStat, Git};
use crate::pr;

pub struct Snapshot {
    pub taken: Instant,
    pub all: Vec<Bead>,
    pub ready: Vec<Bead>,
    pub blocked: HashMap<String, Vec<String>>,
    pub diffstats: HashMap<String, DiffStat>,
    pub conflicts: HashMap<String, Conflict>,
    pub stacked_on: HashMap<String, String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Dependents {
    pub blocks: Vec<String>,
    pub stacked: Vec<String>,
}

pub fn blockers<'a>(
    b: &'a Bead,
    blocked: &'a HashMap<String, Vec<String>>,
    stacked_on: &HashMap<String, String>,
) -> Vec<&'a str> {
    let mut out = b.open_blockers();
    for id in blocked.get(&b.id).into_iter().flatten() {
        if !out.contains(&id.as_str()) {
            out.push(id);
        }
    }
    let parent = stacked_on.get(&b.id);
    out.retain(|id| parent.is_none_or(|p| p != id));
    out
}

pub fn dependents(
    all: &[Bead],
    blocked: &HashMap<String, Vec<String>>,
    stacked_on: &HashMap<String, String>,
) -> HashMap<String, Dependents> {
    let mut map: HashMap<String, Dependents> = HashMap::new();
    let open: HashSet<&str> = all
        .iter()
        .filter(|b| b.status != "closed")
        .map(|b| b.id.as_str())
        .collect();
    for b in all.iter().filter(|b| open.contains(b.id.as_str())) {
        for id in blockers(b, blocked, stacked_on) {
            map.entry(id.to_string())
                .or_default()
                .blocks
                .push(b.id.clone());
        }
        if let Some(p) = stacked_on.get(&b.id) {
            map.entry(p.clone()).or_default().stacked.push(b.id.clone());
        }
    }
    for d in map.values_mut() {
        d.blocks.sort();
        d.stacked.sort();
    }
    map
}

#[derive(Debug, Default, PartialEq)]
pub struct Chain {
    pub ancestors: HashSet<String>,
    pub descendants: HashSet<String>,
}

impl Chain {
    pub fn contains(&self, id: &str) -> bool {
        self.ancestors.contains(id) || self.descendants.contains(id)
    }
}

fn walk<'a>(start: &'a str, next: impl Fn(&str) -> Vec<&'a str>) -> HashSet<String> {
    let mut seen: HashSet<&str> = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        for n in next(id) {
            if seen.insert(n) {
                stack.push(n);
            }
        }
    }
    seen.remove(start);
    seen.into_iter().map(String::from).collect()
}

pub fn chain(
    id: &str,
    all: &[Bead],
    blocked: &HashMap<String, Vec<String>>,
    stacked_on: &HashMap<String, String>,
    dependents: &HashMap<String, Dependents>,
) -> Chain {
    let beads: HashMap<&str, &Bead> = all.iter().map(|b| (b.id.as_str(), b)).collect();
    let ancestors = walk(id, |id| {
        let mut out: Vec<&str> = match beads.get(id) {
            Some(b) => blockers(b, blocked, stacked_on),
            None => blocked
                .get(id)
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect(),
        };
        out.extend(stacked_on.get(id).map(String::as_str));
        out
    });
    let descendants = walk(id, |id| {
        dependents
            .get(id)
            .into_iter()
            .flat_map(|d| d.blocks.iter().chain(&d.stacked))
            .map(String::as_str)
            .collect()
    });
    Chain {
        ancestors,
        descendants,
    }
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

pub const FULL_REFRESH: Duration = Duration::from_secs(60);

struct BdData {
    all: Vec<Bead>,
    ready: Vec<Bead>,
    blocked: HashMap<String, Vec<String>>,
}

struct BdCache {
    stamp: Vec<u8>,
    fetched: Instant,
    data: BdData,
}

impl BdCache {
    fn fresh(&self, stamp: Option<&[u8]>, now: Instant) -> bool {
        stamp == Some(self.stamp.as_slice()) && now.duration_since(self.fetched) < FULL_REFRESH
    }
}

fn fetch(beads: &Beads) -> color_eyre::Result<BdData> {
    let all = beads.list_all()?;
    Ok(BdData {
        ready: beads.ready()?,
        blocked: beads.blocked(&all)?,
        all,
    })
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
        let mut cache: Option<BdCache> = None;
        let mut forced = true;
        loop {
            let taken = Instant::now();
            let stamp = beads.stamp();
            let reuse = !forced
                && cache
                    .as_ref()
                    .is_some_and(|c| c.fresh(stamp.as_deref(), taken));
            if !reuse {
                cache = None;
                match fetch(&beads) {
                    Ok(data) => {
                        cache = Some(BdCache {
                            stamp: stamp.unwrap_or_default(),
                            fetched: taken,
                            data,
                        });
                    }
                    Err(e) => {
                        if tx.send(Event::Snapshot(Err(e.to_string()))).is_err() {
                            return;
                        }
                    }
                }
            }
            if let Some(c) = &cache {
                let d = &c.data;
                let open: HashSet<String> = d
                    .all
                    .iter()
                    .filter(|b| b.status != "closed")
                    .map(|b| b.id.clone())
                    .collect();
                let conflicts = scanner.scan(&git, &base, &open).unwrap_or_else(|e| {
                    tracing::warn!("conflict scan: {e:#}");
                    HashMap::new()
                });
                let snap = Snapshot {
                    taken,
                    diffstats: diffstats(&git, &base, &d.all),
                    all: d.all.clone(),
                    ready: d.ready.clone(),
                    blocked: d.blocked.clone(),
                    conflicts,
                    stacked_on: git.stacks().into_iter().collect(),
                };
                if tx.send(Event::Snapshot(Ok(snap))).is_err() {
                    return;
                }
            }
            forced = match refresh_rx.recv_timeout(every) {
                Ok(()) => {
                    while refresh_rx.try_recv().is_ok() {}
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => return,
            };
        }
    });
    refresh_tx
}

pub fn watched_prs(all: &[Bead]) -> Vec<(String, u64)> {
    all.iter()
        .filter(|b| b.status != "closed" && matches!(b.state(), Some(State::Pr | State::Released)))
        .filter_map(|b| Some((b.id.clone(), b.label("pr")?.parse().ok()?)))
        .collect()
}

pub fn unlinked_releases(all: &[Bead]) -> Vec<String> {
    all.iter()
        .filter(|b| b.status != "closed" && b.state() == Some(State::Released))
        .filter(|b| b.label("pr").is_none())
        .map(|b| b.id.clone())
        .collect()
}

pub fn revisable(all: &[Bead]) -> impl Iterator<Item = &Bead> {
    all.iter()
        .filter(|b| b.status != "closed" && b.state() == Some(State::Revise))
}

pub fn spawn_pr_watcher(beads: Beads, git: Git, every: Duration, tx: Sender<Event>) -> Sender<()> {
    let (refresh_tx, refresh_rx): (Sender<()>, Receiver<()>) = channel();
    let repo = git.repo.clone();
    std::thread::spawn(move || {
        loop {
            match beads.list_all() {
                Ok(all) => {
                    for id in unlinked_releases(&all) {
                        let number = match pr::find(&repo, &git.branch(&id)) {
                            Ok(Some(found)) => found.number,
                            Ok(None) => continue,
                            Err(e) => {
                                tracing::warn!("pr lookup for released {id}: {e:#}");
                                continue;
                            }
                        };
                        let found = Event::Finished(Box::new(move |app| app.pr_found(&id, number)));
                        if tx.send(found).is_err() {
                            return;
                        }
                    }
                    for (id, number) in watched_prs(&all) {
                        let r = pr::status(&repo, number).map_err(|e| e.to_string());
                        if tx.send(Event::Pr(id, number, r)).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => tracing::warn!("pr watcher: {e:#}"),
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
        .filter(|b| {
            !matches!(
                b.state(),
                Some(State::Held | State::Backlog | State::Pr | State::Released)
            )
        })
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
        parked: Vec<String>,
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
            Idle::SlotsFull {
                running,
                max,
                parked,
            } if running < max && !parked.is_empty() => {
                format!(
                    "all {max} slots in use ({running} working, parked: {}; :max <n> to raise)",
                    capped(parked, 3)
                )
            }
            Idle::SlotsFull { running, max, .. } if running < max => {
                format!("all {max} slots in use ({running} working agents, :max <n> to raise)")
            }
            Idle::SlotsFull { running, max, .. } => {
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
    on_pr: bool,
) -> Option<&'a str> {
    let [parent] = blocked.get(id)?.as_slice() else {
        return None;
    };
    all.iter()
        .find(|b| b.id == *parent)
        .filter(|b| {
            b.status != "closed"
                && match b.state() {
                    Some(State::Review | State::Reviewing | State::Released) => true,
                    Some(State::Pr) => on_pr,
                    _ => false,
                }
        })
        .map(|_| parent.as_str())
}

pub fn stackable<'a>(
    all: &'a [Bead],
    blocked: &HashMap<String, Vec<String>>,
    on_pr: bool,
) -> Vec<&'a Bead> {
    all.iter()
        .filter(|b| b.status == "open" && b.state().is_none() && b.issue_type != "epic")
        .filter(|b| stack_parent(&b.id, blocked, all, on_pr).is_some())
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

fn capped(ids: &[String], cap: usize) -> String {
    let shown = ids[..ids.len().min(cap)].join(", ");
    match ids.len().saturating_sub(cap) {
        0 => shown,
        more => format!("{shown} +{more}"),
    }
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
            bead("pr", 0, "2026-01-01T00:00:00Z", &["sulla:pr", "pr:7"]),
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
    fn released_beads_are_never_picked_but_can_be_stacked_on() {
        let mut released = bead("rel", 0, "2026-01-01T00:00:00Z", &["sulla:released"]);
        released.status = "in_progress".into();
        let mut linked = bead("pr", 0, "2026-01-01T00:00:00Z", &["sulla:released", "pr:4"]);
        linked.status = "in_progress".into();
        let all = vec![
            released,
            linked,
            bead("child", 1, "2026-01-01T00:00:00Z", &[]),
        ];
        assert!(pick(&all[..2], &HashSet::new(), &HashSet::new(), 2).is_empty());
        let blocked = HashMap::from([("child".to_string(), vec!["rel".to_string()])]);
        assert_eq!(stack_parent("child", &blocked, &all, false), Some("rel"));
        assert_eq!(unlinked_releases(&all), ["rel"]);
        assert_eq!(watched_prs(&all), [("pr".to_string(), 4)]);
        assert!(orphans(&all, &HashSet::new()).is_empty());
    }

    #[test]
    fn revise_beads_are_picked_within_capacity() {
        let mut revise = bead("rev", 1, "2026-01-01T00:00:00Z", &["sulla:revise", "pr:7"]);
        revise.status = "in_progress".into();
        let mut pr = bead("pr", 0, "2026-01-01T00:00:00Z", &["sulla:pr", "pr:8"]);
        pr.status = "in_progress".into();
        let all = vec![revise, pr];
        let mut candidates = vec![
            bead("p2", 2, "2026-01-01T00:00:00Z", &[]),
            bead("p1", 1, "2026-01-02T00:00:00Z", &[]),
        ];
        candidates.extend(revisable(&all).cloned());
        let ids = |free| -> Vec<String> {
            pick(&candidates, &HashSet::new(), &HashSet::new(), free)
                .iter()
                .map(|b| b.id.clone())
                .collect()
        };
        assert_eq!(ids(1), ["rev"]);
        assert_eq!(ids(2), ["rev", "p1"]);
        assert_eq!(ids(0), Vec::<String>::new());
        let busy = HashSet::from(["rev".to_string()]);
        let picked: Vec<_> = pick(&candidates, &busy, &HashSet::new(), 1)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(picked, ["p1"]);
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
        let pr = bead("pr", 1, "2026-01-01T00:00:00Z", &["sulla:pr"]);

        let base = vec![epic, done, run, pr];
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
    fn slots_full_names_capped_parked_beads() {
        let full = |running, parked: &[&str]| {
            Idle::SlotsFull {
                running,
                max: 5,
                parked: parked.iter().map(|s| s.to_string()).collect(),
            }
            .describe()
        };
        assert_eq!(
            full(1, &["a"]),
            "all 5 slots in use (1 working, parked: a; :max <n> to raise)"
        );
        assert_eq!(
            full(0, &["a", "b", "c", "d", "e"]),
            "all 5 slots in use (0 working, parked: a, b, c +2; :max <n> to raise)"
        );
        assert_eq!(
            full(3, &[]),
            "all 5 slots in use (3 working agents, :max <n> to raise)"
        );
        assert_eq!(
            full(5, &[]),
            "slots full, 5/5 agents running (:max <n> to raise)"
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
        assert_eq!(stack_parent("on-rev", &blocked, &all, false), Some("rev"));
        assert_eq!(stack_parent("on-run", &blocked, &all, false), None);
        assert_eq!(stack_parent("on-both", &blocked, &all, false), None);
        let ids: Vec<_> = stackable(&all, &blocked, false)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, ["on-rev"]);
    }

    #[test]
    fn dependents_reverse_blockers_and_stacks() {
        let t = "2026-01-01T00:00:00Z";
        let mut dep = bead("dep", 1, t, &[]);
        dep.dependencies = vec![
            crate::beads::Dependency {
                id: "a".into(),
                status: "open".into(),
                dependency_type: "blocks".into(),
            },
            crate::beads::Dependency {
                id: "x".into(),
                status: "closed".into(),
                dependency_type: "blocks".into(),
            },
        ];
        let mut done = bead("done", 1, t, &[]);
        done.status = "closed".into();
        let all = vec![
            bead("a", 1, t, &[]),
            bead("b", 1, t, &[]),
            dep,
            bead("s1", 1, t, &[]),
            bead("s2", 1, t, &[]),
            done,
        ];
        let blocked = HashMap::from([
            ("dep".to_string(), vec!["a".to_string(), "b".to_string()]),
            ("s1".to_string(), vec!["a".to_string()]),
            ("s2".to_string(), vec!["a".to_string()]),
            ("done".to_string(), vec!["a".to_string()]),
        ]);
        let stacked_on = HashMap::from([
            ("s1".to_string(), "a".to_string()),
            ("s2".to_string(), "a".to_string()),
        ]);
        let map = dependents(&all, &blocked, &stacked_on);
        assert_eq!(
            map.get("a"),
            Some(&Dependents {
                blocks: vec!["dep".into()],
                stacked: vec!["s1".into(), "s2".into()],
            })
        );
        assert_eq!(
            map.get("b").map(|d| &d.blocks),
            Some(&vec!["dep".to_string()])
        );
        assert!(!map.contains_key("x"));
        assert!(blockers(&all[3], &blocked, &stacked_on).is_empty());
    }

    #[test]
    fn stacks_on_awaiting_pr_parent_only_with_pr_workflow() {
        let mut merged = bead("merged", 1, "2026-01-01T00:00:00Z", &["sulla:pr", "pr:2"]);
        merged.status = "closed".into();
        let all = vec![
            bead("pr", 1, "2026-01-01T00:00:00Z", &["sulla:pr", "pr:1"]),
            bead("rev", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
            merged,
            bead("on-pr", 1, "2026-01-01T00:00:00Z", &[]),
            bead("on-rev", 1, "2026-01-01T00:00:00Z", &[]),
            bead("on-both", 1, "2026-01-01T00:00:00Z", &[]),
            bead("on-merged", 1, "2026-01-01T00:00:00Z", &[]),
        ];
        let blocked = HashMap::from([
            ("on-pr".to_string(), vec!["pr".to_string()]),
            ("on-rev".to_string(), vec!["rev".to_string()]),
            (
                "on-both".to_string(),
                vec!["pr".to_string(), "rev".to_string()],
            ),
            ("on-merged".to_string(), vec!["merged".to_string()]),
        ]);
        assert_eq!(stack_parent("on-pr", &blocked, &all, true), Some("pr"));
        assert_eq!(stack_parent("on-pr", &blocked, &all, false), None);
        assert_eq!(stack_parent("on-rev", &blocked, &all, true), Some("rev"));
        assert_eq!(stack_parent("on-both", &blocked, &all, true), None);
        assert_eq!(stack_parent("on-merged", &blocked, &all, true), None);
        let ids = |on_pr| -> Vec<&str> {
            stackable(&all, &blocked, on_pr)
                .iter()
                .map(|b| b.id.as_str())
                .collect()
        };
        assert_eq!(ids(true), ["on-pr", "on-rev"]);
        assert_eq!(ids(false), ["on-rev"]);
    }

    #[test]
    fn chain_walks_ancestors_and_descendants_with_cycle_guard() {
        let t = "2026-01-01T00:00:00Z";
        let all: Vec<Bead> = ["root", "mid", "leaf", "stack", "side", "x", "y"]
            .iter()
            .map(|id| bead(id, 1, t, &[]))
            .collect();
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let blocked = HashMap::from([
            ("mid".to_string(), ids(&["root"])),
            ("leaf".to_string(), ids(&["mid", "side"])),
            ("x".to_string(), ids(&["y"])),
            ("y".to_string(), ids(&["x"])),
        ]);
        let stacked_on = HashMap::from([("stack".to_string(), "leaf".to_string())]);
        let deps = dependents(&all, &blocked, &stacked_on);
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();

        let c = chain("mid", &all, &blocked, &stacked_on, &deps);
        assert_eq!(c.ancestors, set(&["root"]));
        assert_eq!(c.descendants, set(&["leaf", "stack"]));
        assert!(!c.contains("side") && !c.contains("mid"));

        let c = chain("stack", &all, &blocked, &stacked_on, &deps);
        assert_eq!(c.ancestors, set(&["leaf", "mid", "side", "root"]));
        assert!(c.descendants.is_empty());

        let c = chain("x", &all, &blocked, &stacked_on, &deps);
        assert_eq!(c.ancestors, set(&["y"]));
        assert_eq!(c.descendants, set(&["y"]));
    }

    #[test]
    fn reviewing_bead_is_orphaned_only_without_reviewer() {
        let all = vec![
            bead("run", 1, "2026-01-01T00:00:00Z", &["sulla:running"]),
            bead("rev", 1, "2026-01-01T00:00:00Z", &["sulla:reviewing"]),
            bead("live", 1, "2026-01-01T00:00:00Z", &["sulla:reviewing"]),
            bead("done", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
            bead("idle", 1, "2026-01-01T00:00:00Z", &["sulla:review"]),
            bead("pr", 1, "2026-01-01T00:00:00Z", &["sulla:pr"]),
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

    #[test]
    fn bd_cache_is_fresh_until_the_stamp_changes_or_it_ages_out() {
        let fetched = Instant::now();
        let c = BdCache {
            stamp: b"root-a".to_vec(),
            fetched,
            data: BdData {
                all: Vec::new(),
                ready: Vec::new(),
                blocked: HashMap::new(),
            },
        };
        assert!(c.fresh(Some(b"root-a"), fetched + Duration::from_secs(2)));
        assert!(!c.fresh(Some(b"root-b"), fetched + Duration::from_secs(2)));
        assert!(!c.fresh(None, fetched + Duration::from_secs(2)));
        assert!(!c.fresh(Some(b"root-a"), fetched + FULL_REFRESH));
    }
}
