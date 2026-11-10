use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{NotifyConfig, NotifyMethod, Trigger, WhenPresent};

pub const BELL_GLYPH: &str = "🔔";

pub fn present(focused: Option<bool>, idle: Duration, since_input: Duration) -> bool {
    focused != Some(false) && since_input < idle
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pending {
    trigger: Trigger,
    id: String,
    title: String,
}

pub struct Notifier {
    cfg: NotifyConfig,
    tmux: bool,
    flagged: BTreeMap<String, Trigger>,
    last: HashMap<String, Instant>,
    pending: Vec<Pending>,
    batch_until: Option<Instant>,
    unseen: BTreeSet<String>,
    #[cfg(test)]
    pub fired: Vec<(Trigger, String)>,
    #[cfg(test)]
    pub sent: Vec<(String, String)>,
}

impl Notifier {
    pub fn new(cfg: NotifyConfig) -> Self {
        let n = Self {
            tmux: uses_tmux(&cfg),
            cfg,
            flagged: BTreeMap::new(),
            last: HashMap::new(),
            pending: Vec::new(),
            batch_until: None,
            unseen: BTreeSet::new(),
            #[cfg(test)]
            fired: Vec::new(),
            #[cfg(test)]
            sent: Vec::new(),
        };
        n.sync_tmux();
        n
    }

    pub fn reconfigure(&mut self, cfg: NotifyConfig) {
        let flagged = std::mem::take(&mut self.flagged);
        self.sync_tmux();
        self.tmux = uses_tmux(&cfg);
        self.cfg = cfg;
        self.flagged = flagged;
        self.sync_tmux();
    }

    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.cfg.idle_secs)
    }

    pub fn fire(
        &mut self,
        trigger: Trigger,
        id: &str,
        title: &str,
        present: bool,
        now: Instant,
    ) -> bool {
        if !self.should_fire(trigger, id, now) {
            return false;
        }
        tracing::info!("notify: {}", line(trigger, id));
        #[cfg(test)]
        self.fired.push((trigger, id.to_string()));
        self.sync_tmux();
        if present && self.cfg.when_present == WhenPresent::Quiet {
            return true;
        }
        if !present {
            self.unseen.insert(id.to_string());
        }
        self.queue(trigger, id, title, now);
        self.flush(now);
        true
    }

    fn queue(&mut self, trigger: Trigger, id: &str, title: &str, now: Instant) {
        match self.pending.iter_mut().find(|p| p.id == id) {
            Some(p) if rank(trigger) >= rank(p.trigger) => {
                p.trigger = trigger;
                p.title = title.to_string();
            }
            Some(_) => {}
            None => self.pending.push(Pending {
                trigger,
                id: id.to_string(),
                title: title.to_string(),
            }),
        }
        self.batch_until
            .get_or_insert(now + Duration::from_secs(self.cfg.batch_secs));
    }

    pub fn flush(&mut self, now: Instant) {
        if self.batch_until.is_none_or(|t| now < t) {
            return;
        }
        self.batch_until = None;
        let mut batch = std::mem::take(&mut self.pending);
        batch.retain(|p| self.flagged.contains_key(&p.id));
        let Some((summary, body)) = compose(&batch) else {
            return;
        };
        tracing::info!("notify sent: {summary}");
        #[cfg(test)]
        self.sent.push((summary.clone(), body.clone()));
        for m in &self.cfg.methods {
            match m {
                NotifyMethod::Bell => {
                    let mut out = std::io::stdout();
                    let _ = out.write_all(b"\x07").and_then(|_| out.flush());
                }
                NotifyMethod::Desktop => {
                    let mut cmd = Command::new(&self.cfg.notify_send);
                    cmd.args(["-a", "sulla", &summary, &body]);
                    detach(cmd);
                }
                NotifyMethod::Tmux => {}
            }
        }
    }

    pub fn welcome_back(&mut self) -> Option<String> {
        self.pending.clear();
        self.batch_until = None;
        let unseen = std::mem::take(&mut self.unseen);
        let mut counts: Vec<(&str, u8, usize)> = Vec::new();
        for t in unseen.iter().filter_map(|id| self.flagged.get(id)) {
            let label = short(*t);
            match counts.iter_mut().find(|(l, _, _)| *l == label) {
                Some(c) => c.2 += 1,
                None => counts.push((label, rank(*t), 1)),
            }
        }
        if counts.is_empty() {
            return None;
        }
        counts.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)));
        let parts: Vec<String> = counts.iter().map(|(l, _, n)| format!("{n} {l}")).collect();
        Some(format!("while you were away: {}", parts.join(", ")))
    }

    fn should_fire(&mut self, trigger: Trigger, id: &str, now: Instant) -> bool {
        if !self.cfg.on.contains(&trigger) {
            return false;
        }
        let cooldown = Duration::from_secs(self.cfg.cooldown_secs);
        let flag = self.flagged.get(id).copied();
        let recent = self
            .last
            .get(id)
            .is_some_and(|at| now.saturating_duration_since(*at) < cooldown);
        if recent && flag.is_some_and(|f| rank(f) >= rank(trigger)) {
            return false;
        }
        self.last.insert(id.to_string(), now);
        let keep = flag.filter(|f| rank(*f) > rank(trigger)).unwrap_or(trigger);
        self.flagged.insert(id.to_string(), keep);
        true
    }

    pub fn clear(&mut self, id: &str) {
        self.last.remove(id);
        self.unseen.remove(id);
        if self.flagged.remove(id).is_some() {
            self.sync_tmux();
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, Trigger) -> bool) {
        let before = self.flagged.len();
        self.flagged.retain(|id, t| keep(id, *t));
        let flagged = &self.flagged;
        self.unseen.retain(|id| flagged.contains_key(id));
        if self.flagged.len() != before {
            self.sync_tmux();
        }
    }

    pub fn shutdown(&mut self) {
        self.flagged.clear();
        self.sync_tmux();
    }

    fn tmux_value(&self) -> Option<String> {
        (!self.flagged.is_empty()).then(|| format!("sulla:{}", self.flagged.len()))
    }

    fn sync_tmux(&self) {
        if !self.tmux {
            return;
        }
        let mut cmd = Command::new("tmux");
        match self.tmux_value() {
            Some(v) => cmd.args(["set-option", "-q", &self.cfg.tmux_option, &v]),
            None => cmd.args(["set-option", "-qu", &self.cfg.tmux_option]),
        };
        detach(cmd);
    }
}

fn uses_tmux(cfg: &NotifyConfig) -> bool {
    cfg.methods.contains(&NotifyMethod::Tmux) && std::env::var_os("TMUX").is_some()
}

fn rank(trigger: Trigger) -> u8 {
    match trigger {
        Trigger::Dead => 6,
        Trigger::Budget => 5,
        Trigger::Waiting | Trigger::Chat => 4,
        Trigger::Stalled => 3,
        Trigger::Review => 2,
        Trigger::Pr => 1,
    }
}

fn what(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Review => "ready for review",
        Trigger::Dead => "agent died",
        Trigger::Stalled => "stalled",
        Trigger::Waiting | Trigger::Chat => "waiting for input",
        Trigger::Budget => "over budget, held",
        Trigger::Pr => "has PR activity",
    }
}

fn short(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Review => "ready for review",
        Trigger::Dead => "dead",
        Trigger::Stalled => "stalled",
        Trigger::Waiting | Trigger::Chat => "waiting",
        Trigger::Budget => "over budget",
        Trigger::Pr => "with PR activity",
    }
}

pub fn line(trigger: Trigger, id: &str) -> String {
    format!("{id} {}", what(trigger))
}

fn compose(batch: &[Pending]) -> Option<(String, String)> {
    match batch {
        [] => None,
        [p] => Some((
            format!("sulla: {}", line(p.trigger, &p.id)),
            p.title.clone(),
        )),
        _ => {
            let body: Vec<String> = batch.iter().map(|p| line(p.trigger, &p.id)).collect();
            Some((format!("sulla: {} need you", batch.len()), body.join("\n")))
        }
    }
}

fn detach(mut cmd: Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!("notify: {:?}: {e}", cmd.get_program()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notifier(on: Vec<Trigger>) -> Notifier {
        Notifier::new(NotifyConfig {
            methods: Vec::new(),
            on,
            ..NotifyConfig::default()
        })
    }

    #[test]
    fn skips_triggers_not_enabled() {
        let mut n = notifier(vec![Trigger::Review]);
        assert!(!n.should_fire(Trigger::Waiting, "a", Instant::now()));
        assert!(n.should_fire(Trigger::Review, "a", Instant::now()));
        assert_eq!(n.tmux_value().as_deref(), Some("sulla:1"));
    }

    #[test]
    fn cooldown_keeps_the_most_actionable_notification() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        assert!(n.should_fire(Trigger::Pr, "a", t0));
        assert!(!n.should_fire(Trigger::Pr, "a", at(5)));
        assert!(n.should_fire(Trigger::Review, "a", at(6)));
        assert!(!n.should_fire(Trigger::Pr, "a", at(7)));
        assert!(n.should_fire(Trigger::Waiting, "a", at(8)));
        assert!(!n.should_fire(Trigger::Review, "a", at(9)));
        assert!(n.should_fire(Trigger::Budget, "a", at(10)));
        assert!(n.should_fire(Trigger::Dead, "a", at(11)));
        assert!(!n.should_fire(Trigger::Waiting, "a", at(12)));
        assert_eq!(n.flagged["a"], Trigger::Dead);
        assert!(n.should_fire(Trigger::Review, "a", at(80)));
        assert_eq!(n.flagged["a"], Trigger::Dead);
        assert!(n.should_fire(Trigger::Waiting, "b", t0));
        assert_eq!(n.tmux_value().as_deref(), Some("sulla:2"));
    }

    #[test]
    fn a_resolved_flag_no_longer_suppresses_lesser_triggers() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        assert!(n.should_fire(Trigger::Waiting, "a", t0));
        n.retain(|_, t| t != Trigger::Waiting);
        assert!(n.should_fire(Trigger::Review, "a", t0 + Duration::from_secs(2)));
        assert_eq!(n.flagged["a"], Trigger::Review);
        n.clear("a");
        assert!(n.should_fire(Trigger::Review, "a", t0 + Duration::from_secs(3)));
    }

    #[test]
    fn defaults_fire_dead_but_not_stalled_or_chat() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        assert!(n.should_fire(Trigger::Dead, "a", t0));
        assert!(!n.should_fire(Trigger::Stalled, "b", t0));
        assert!(!n.should_fire(Trigger::Chat, "c", t0));
    }

    #[test]
    fn clear_and_retain_drop_flags() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        n.should_fire(Trigger::Review, "a", t0);
        n.should_fire(Trigger::Waiting, "b", t0);
        n.should_fire(Trigger::Waiting, "c", t0);
        n.clear("a");
        n.retain(|id, _| id != "b");
        assert_eq!(n.flagged.keys().collect::<Vec<_>>(), ["c"]);
        n.clear("c");
        assert_eq!(n.tmux_value(), None);
    }

    #[test]
    fn reconfigure_keeps_flags_and_applies_new_triggers() {
        let mut n = notifier(vec![Trigger::Review]);
        let t0 = Instant::now();
        assert!(n.should_fire(Trigger::Review, "a", t0));
        n.reconfigure(NotifyConfig {
            methods: Vec::new(),
            on: vec![Trigger::Waiting],
            ..NotifyConfig::default()
        });
        assert_eq!(n.tmux_value().as_deref(), Some("sulla:1"));
        assert!(!n.should_fire(Trigger::Review, "b", t0));
        assert!(n.should_fire(Trigger::Waiting, "b", t0));
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn presence_needs_focus_and_recent_input() {
        let idle = secs(120);
        assert!(present(Some(true), idle, secs(5)));
        assert!(!present(Some(true), idle, secs(121)));
        assert!(!present(Some(false), idle, secs(5)));
        assert!(present(None, idle, secs(5)));
        assert!(!present(None, idle, secs(120)));
    }

    #[test]
    fn quiet_while_present_unless_always() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        assert!(n.fire(Trigger::Review, "a", "A", true, t0));
        n.flush(t0 + secs(60));
        assert!(n.sent.is_empty());
        assert_eq!(n.tmux_value().as_deref(), Some("sulla:1"));
        assert_eq!(n.welcome_back(), None);

        n.reconfigure(NotifyConfig {
            methods: Vec::new(),
            when_present: WhenPresent::Always,
            ..NotifyConfig::default()
        });
        assert!(n.fire(Trigger::Review, "b", "B", true, t0));
        n.flush(t0 + secs(5));
        assert_eq!(n.sent, [("sulla: b ready for review".into(), "B".into())]);
        assert_eq!(n.welcome_back(), None);
    }

    #[test]
    fn a_burst_is_sent_once_and_a_single_event_keeps_its_message() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        assert!(n.fire(Trigger::Review, "a", "A", false, t0));
        n.flush(t0 + secs(4));
        assert!(n.sent.is_empty());
        n.flush(t0 + secs(5));
        assert_eq!(n.sent, [("sulla: a ready for review".into(), "A".into())]);

        n.sent.clear();
        let t1 = t0 + secs(10);
        n.fire(Trigger::Review, "b", "B", false, t1);
        n.fire(Trigger::Waiting, "c", "C", false, t1 + secs(1));
        n.fire(Trigger::Pr, "d", "D", false, t1 + secs(2));
        n.fire(Trigger::Dead, "c", "C", false, t1 + secs(3));
        n.flush(t1 + secs(4));
        assert!(n.sent.is_empty());
        n.flush(t1 + secs(5));
        n.flush(t1 + secs(10));
        assert_eq!(
            n.sent,
            [(
                "sulla: 3 need you".into(),
                "b ready for review\nc agent died\nd has PR activity".into()
            )]
        );
    }

    #[test]
    fn zero_batch_sends_at_once_and_resolved_flags_are_dropped() {
        let mut n = notifier(NotifyConfig::default().on);
        n.cfg.batch_secs = 0;
        let t0 = Instant::now();
        n.fire(Trigger::Waiting, "a", "A", false, t0);
        assert_eq!(n.sent.len(), 1);
        n.cfg.batch_secs = 5;
        n.fire(Trigger::Waiting, "b", "B", false, t0);
        n.retain(|id, _| id != "b");
        n.flush(t0 + secs(5));
        assert_eq!(n.sent.len(), 1);
    }

    #[test]
    fn welcome_back_summarises_what_fired_while_away() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        n.fire(Trigger::Review, "a", "A", false, t0);
        n.fire(Trigger::Waiting, "b", "B", false, t0);
        n.fire(Trigger::Review, "c", "C", false, t0);
        n.fire(Trigger::Review, "d", "D", true, t0);
        n.fire(Trigger::Pr, "e", "E", false, t0);
        n.clear("e");
        assert_eq!(
            n.welcome_back().as_deref(),
            Some("while you were away: 2 ready for review, 1 waiting")
        );
        n.flush(t0 + secs(5));
        assert!(n.sent.is_empty());
        assert_eq!(n.welcome_back(), None);
    }
}
