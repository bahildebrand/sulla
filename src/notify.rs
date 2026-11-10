use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{NotifyConfig, NotifyMethod, Trigger};

pub struct Notifier {
    cfg: NotifyConfig,
    tmux: bool,
    flagged: BTreeMap<String, Trigger>,
    last: HashMap<String, Instant>,
    #[cfg(test)]
    pub fired: Vec<(Trigger, String)>,
}

impl Notifier {
    pub fn new(cfg: NotifyConfig) -> Self {
        let n = Self {
            tmux: uses_tmux(&cfg),
            cfg,
            flagged: BTreeMap::new(),
            last: HashMap::new(),
            #[cfg(test)]
            fired: Vec::new(),
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

    pub fn fire(&mut self, trigger: Trigger, id: &str, title: &str) -> bool {
        if !self.should_fire(trigger, id, Instant::now()) {
            return false;
        }
        let (summary, body) = message(trigger, id, title);
        tracing::info!("notify: {summary}");
        #[cfg(test)]
        self.fired.push((trigger, id.to_string()));
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
        self.sync_tmux();
        true
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
        if self.flagged.remove(id).is_some() {
            self.sync_tmux();
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&str, Trigger) -> bool) {
        let before = self.flagged.len();
        self.flagged.retain(|id, t| keep(id, *t));
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

fn message(trigger: Trigger, id: &str, title: &str) -> (String, String) {
    let what = match trigger {
        Trigger::Review => "ready for review",
        Trigger::Dead => "agent died",
        Trigger::Stalled => "stalled",
        Trigger::Waiting | Trigger::Chat => "waiting for input",
        Trigger::Budget => "over budget, held",
        Trigger::Pr => "has PR activity",
    };
    (format!("sulla: {id} {what}"), title.to_string())
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
}
