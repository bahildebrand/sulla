use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{NotifyConfig, NotifyMethod, Trigger};

pub struct Notifier {
    cfg: NotifyConfig,
    tmux: bool,
    flagged: BTreeMap<String, Trigger>,
    last: HashMap<String, (Trigger, Instant)>,
}

impl Notifier {
    pub fn new(cfg: NotifyConfig) -> Self {
        let tmux = cfg.methods.contains(&NotifyMethod::Tmux) && std::env::var_os("TMUX").is_some();
        let n = Self {
            cfg,
            tmux,
            flagged: BTreeMap::new(),
            last: HashMap::new(),
        };
        n.sync_tmux();
        n
    }

    pub fn fire(&mut self, trigger: Trigger, id: &str, title: &str) -> bool {
        if !self.should_fire(trigger, id, Instant::now()) {
            return false;
        }
        let (summary, body) = message(trigger, id, title);
        tracing::info!("notify: {summary}");
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
        if let Some((t, at)) = self.last.get(id)
            && now.duration_since(*at) < cooldown
            && (*t == trigger || trigger == Trigger::Waiting)
        {
            return false;
        }
        self.last.insert(id.to_string(), (trigger, now));
        self.flagged.insert(id.to_string(), trigger);
        true
    }

    pub fn clear(&mut self, id: &str) {
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

fn message(trigger: Trigger, id: &str, title: &str) -> (String, String) {
    let what = match trigger {
        Trigger::Review => "ready for review",
        Trigger::Stalled => "stalled",
        Trigger::Waiting => "waiting for input",
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
    fn cooldown_dedupes_and_waiting_yields_to_others() {
        let mut n = notifier(NotifyConfig::default().on);
        let t0 = Instant::now();
        assert!(n.should_fire(Trigger::Waiting, "a", t0));
        assert!(!n.should_fire(Trigger::Waiting, "a", t0 + Duration::from_secs(5)));
        assert!(n.should_fire(Trigger::Review, "a", t0 + Duration::from_secs(6)));
        assert!(!n.should_fire(Trigger::Waiting, "a", t0 + Duration::from_secs(7)));
        assert!(!n.should_fire(Trigger::Review, "a", t0 + Duration::from_secs(8)));
        assert!(n.should_fire(Trigger::Review, "a", t0 + Duration::from_secs(70)));
        assert!(n.should_fire(Trigger::Waiting, "b", t0));
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
}
