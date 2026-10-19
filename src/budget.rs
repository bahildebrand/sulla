use std::time::{Duration, Instant};

use crate::beads::Bead;
use crate::config::BudgetConfig;
use crate::usage::{Usage, tokens};

const LABEL: &str = "budget:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Budget {
    pub minutes: u64,
    pub tokens: u64,
}

impl Budget {
    pub fn for_bead(cfg: &BudgetConfig, bead: &Bead) -> Self {
        let mut b = Self {
            minutes: cfg.minutes,
            tokens: cfg.tokens,
        };
        for l in &bead.labels {
            let Some((key, val)) = l.strip_prefix(LABEL).and_then(|r| r.split_once('=')) else {
                continue;
            };
            match (key.trim(), parse_count(val)) {
                ("minutes", Some(n)) => b.minutes = n,
                ("tokens", Some(n)) => b.tokens = n,
                _ => {}
            }
        }
        b
    }

    pub fn is_unlimited(&self) -> bool {
        self.minutes == 0 && self.tokens == 0
    }

    pub fn progress(&self, elapsed: Duration, used: u64) -> String {
        let mut parts = Vec::new();
        if self.minutes > 0 {
            parts.push(format!("{}/{}m", elapsed.as_secs() / 60, self.minutes));
        }
        if self.tokens > 0 {
            parts.push(format!("{}/{} tok", tokens(used), tokens(self.tokens)));
        }
        format!("budget {}", parts.join(" "))
    }

    pub fn exceeded(&self, elapsed: Duration, used: u64) -> Option<String> {
        let mins = elapsed.as_secs() / 60;
        if self.minutes > 0 && elapsed >= Duration::from_secs(self.minutes * 60) {
            return Some(format!(
                "time budget exceeded: {mins}m of {}m",
                self.minutes
            ));
        }
        if self.tokens > 0 && used >= self.tokens {
            return Some(format!(
                "token budget exceeded: {} of {} tokens",
                tokens(used),
                tokens(self.tokens)
            ));
        }
        None
    }
}

fn parse_count(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.char_indices().last()? {
        (i, 'k' | 'K') => (&s[..i], 1e3),
        (i, 'm' | 'M') => (&s[..i], 1e6),
        _ => (s, 1.0),
    };
    let n: f64 = num.parse().ok()?;
    (n >= 0.0 && n.is_finite()).then(|| (n * mul).round() as u64)
}

pub struct Run {
    start: Instant,
    base: Option<Usage>,
}

impl Run {
    pub fn new(now: Instant, base: Option<Usage>) -> Self {
        Self { start: now, base }
    }

    pub fn observe(&mut self, current: Usage) {
        self.base.get_or_insert(current);
    }

    pub fn used(&self, current: Usage) -> u64 {
        self.base.map_or(0, |b| {
            (current.input + current.output).saturating_sub(b.input + b.output)
        })
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        now.duration_since(self.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(labels: &[&str]) -> Bead {
        let labels: Vec<String> = labels.iter().map(|s| format!("\"{s}\"")).collect();
        serde_json::from_str(&format!(
            "{{\"id\":\"x-1\",\"title\":\"t\",\"status\":\"open\",\"labels\":[{}]}}",
            labels.join(",")
        ))
        .unwrap()
    }

    #[test]
    fn labels_override_config() {
        let cfg = BudgetConfig {
            minutes: 30,
            tokens: 1_000,
        };
        assert_eq!(
            Budget::for_bead(&cfg, &bead(&[])),
            Budget {
                minutes: 30,
                tokens: 1_000
            }
        );
        let b = Budget::for_bead(
            &cfg,
            &bead(&["budget:tokens=1.5M", "budget:minutes=0", "budget:minutes=x"]),
        );
        assert_eq!(
            b,
            Budget {
                minutes: 0,
                tokens: 1_500_000
            }
        );
        assert_eq!(parse_count("200k"), Some(200_000));
        assert_eq!(parse_count("-1"), None);
    }

    #[test]
    fn exceeded_checks_each_limit() {
        let b = Budget {
            minutes: 10,
            tokens: 0,
        };
        assert_eq!(b.exceeded(Duration::from_secs(599), u64::MAX), None);
        assert_eq!(
            b.exceeded(Duration::from_secs(600), 0).as_deref(),
            Some("time budget exceeded: 10m of 10m")
        );
        let b = Budget {
            minutes: 0,
            tokens: 200_000,
        };
        assert_eq!(b.exceeded(Duration::from_secs(1 << 30), 199_999), None);
        assert_eq!(
            b.exceeded(Duration::ZERO, 250_000).as_deref(),
            Some("token budget exceeded: 250.0k of 200.0k tokens")
        );
    }

    #[test]
    fn run_counts_tokens_from_first_baseline() {
        let u = |input, output| Usage {
            input,
            output,
            ..Default::default()
        };
        let mut r = Run::new(Instant::now(), None);
        assert_eq!(r.used(u(1_000, 100)), 0);
        r.observe(u(1_000, 100));
        r.observe(u(1_200, 100));
        assert_eq!(r.used(u(1_500, 200)), 600);
        let r = Run::new(Instant::now(), Some(Usage::default()));
        assert_eq!(r.used(u(10, 5)), 15);
    }
}
