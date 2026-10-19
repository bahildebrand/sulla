use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::AddAssign;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::app::Event;
use crate::config::AgentKind;

const RESOLVE_EVERY: Duration = Duration::from_secs(15);

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub credits: f64,
    pub usd: f64,
}

impl AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.input += o.input;
        self.cached += o.cached;
        self.output += o.output;
        self.credits += o.credits;
        self.usd += o.usd;
    }
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.credits == 0.0 && self.usd == 0.0
    }

    fn minus(self, o: Self) -> Self {
        Self {
            input: self.input.saturating_sub(o.input),
            cached: self.cached.saturating_sub(o.cached),
            output: self.output.saturating_sub(o.output),
            credits: (self.credits - o.credits).max(0.0),
            usd: (self.usd - o.usd).max(0.0),
        }
    }

    pub fn short(&self) -> String {
        let mut s = format!("{} in {} out", tokens(self.input), tokens(self.output));
        if self.credits > 0.0 {
            s.push_str(&format!(" {:.1} cr", self.credits));
        }
        if self.usd > 0.0 {
            s.push_str(&format!(" ${:.2}", self.usd));
        }
        s
    }

    pub fn detail(&self) -> String {
        let mut s = format!(
            "{} input ({} cached), {} output",
            self.input, self.cached, self.output
        );
        if self.credits > 0.0 {
            s.push_str(&format!(", {:.2} AI credits", self.credits));
        }
        if self.usd > 0.0 {
            s.push_str(&format!(", ${:.2}", self.usd));
        }
        s
    }
}

pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn copilot_line(v: &Value) -> Option<Usage> {
    if v.get("type")?.as_str()? != "session.usage_record" {
        return None;
    }
    let usage = v.get("data")?.get("usage")?;
    let nano = usage
        .get("copilotUsage")
        .and_then(|c| c.get("totalNanoAiu"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    Some(Usage {
        input: u(usage, "inputTokens"),
        cached: u(usage, "cacheReadTokens"),
        output: u(usage, "outputTokens"),
        credits: nano / 1e9,
        usd: 0.0,
    })
}

fn claude_line(v: &Value) -> Option<(String, Usage)> {
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| v.get("uuid").and_then(Value::as_str))?
        .to_string();
    let cached = u(usage, "cache_read_input_tokens");
    Some((
        id,
        Usage {
            input: u(usage, "input_tokens") + u(usage, "cache_creation_input_tokens") + cached,
            cached,
            output: u(usage, "output_tokens"),
            credits: 0.0,
            usd: v.get("costUSD").and_then(Value::as_f64).unwrap_or(0.0),
        },
    ))
}

#[derive(Default)]
struct Transcript {
    path: Option<PathBuf>,
    tried: Option<Instant>,
    offset: u64,
    partial: String,
    total: Usage,
    messages: HashMap<String, Usage>,
}

impl Transcript {
    fn usage(&self) -> Usage {
        let mut u = self.total;
        for m in self.messages.values() {
            u += *m;
        }
        u
    }

    fn reset(&mut self) {
        let path = self.path.take();
        *self = Self {
            path,
            ..Self::default()
        };
    }

    fn feed(&mut self, kind: AgentKind, chunk: &str) {
        self.partial.push_str(chunk);
        let Some(end) = self.partial.rfind('\n') else {
            return;
        };
        let rest = self.partial.split_off(end + 1);
        let lines = std::mem::replace(&mut self.partial, rest);
        for line in lines.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match kind {
                AgentKind::Copilot => {
                    if let Some(u) = copilot_line(&v) {
                        self.total += u;
                    }
                }
                AgentKind::Claude => {
                    if let Some((id, u)) = claude_line(&v) {
                        let e = self.messages.entry(id).or_default();
                        e.input = e.input.max(u.input);
                        e.cached = e.cached.max(u.cached);
                        e.output = e.output.max(u.output);
                        e.usd = e.usd.max(u.usd);
                    }
                }
            }
        }
    }

    fn poll(&mut self, kind: AgentKind, locate: impl FnOnce() -> Option<PathBuf>) {
        if self.path.is_none() {
            if self.tried.is_some_and(|t| t.elapsed() < RESOLVE_EVERY) {
                return;
            }
            self.tried = Some(Instant::now());
            self.path = locate();
        }
        let Some(path) = &self.path else { return };
        let Ok(mut f) = File::open(path) else { return };
        let Ok(len) = f.metadata().map(|m| m.len()) else {
            return;
        };
        if len < self.offset {
            self.reset();
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.take(len - self.offset).read_to_end(&mut buf).is_err() {
            return;
        }
        self.offset += buf.len() as u64;
        self.feed(kind, &String::from_utf8_lossy(&buf));
    }
}

struct Locator {
    copilot: PathBuf,
    claude: PathBuf,
}

impl Locator {
    fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let copilot = std::env::var_os("COPILOT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".copilot"));
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        Self { copilot, claude }
    }

    fn find(&self, kind: AgentKind, session: &str) -> Option<PathBuf> {
        match kind {
            AgentKind::Copilot => {
                let p = self
                    .copilot
                    .join("session-state")
                    .join(session)
                    .join("events.jsonl");
                p.is_file().then_some(p)
            }
            AgentKind::Claude => {
                let name = format!("{session}.jsonl");
                std::fs::read_dir(self.claude.join("projects"))
                    .ok()?
                    .flatten()
                    .map(|d| d.path().join(&name))
                    .find(|p| p.is_file())
            }
        }
    }
}

pub type Watch = Vec<(String, Vec<String>)>;

pub struct Report {
    pub per_key: HashMap<String, Usage>,
    pub run: Usage,
}

struct Tracker {
    kind: AgentKind,
    locator: Locator,
    sessions: HashMap<String, Transcript>,
    baseline: HashMap<String, Usage>,
}

impl Tracker {
    fn scan(&mut self, watch: &Watch) -> Report {
        let mut per_key = HashMap::new();
        let mut run = Usage::default();
        for (key, sessions) in watch {
            let mut total = Usage::default();
            for s in sessions {
                let t = self.sessions.entry(s.clone()).or_default();
                t.poll(self.kind, || self.locator.find(self.kind, s));
                let now = t.usage();
                let base = *self.baseline.entry(s.clone()).or_insert(now);
                run += now.minus(base);
                total += now;
            }
            if !total.is_empty() {
                per_key.insert(key.clone(), total);
            }
        }
        Report { per_key, run }
    }
}

pub fn spawn(kind: AgentKind, every: Duration, tx: Sender<Event>) -> Sender<Watch> {
    let (watch_tx, watch_rx): (Sender<Watch>, Receiver<Watch>) = channel();
    std::thread::spawn(move || {
        let mut tracker = Tracker {
            kind,
            locator: Locator::from_env(),
            sessions: HashMap::new(),
            baseline: HashMap::new(),
        };
        let mut watch = Watch::new();
        let mut last: Option<(HashMap<String, Usage>, Usage)> = None;
        loop {
            match watch_rx.recv_timeout(every) {
                Ok(w) => {
                    watch = w;
                    while let Ok(w) = watch_rx.try_recv() {
                        watch = w;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let r = tracker.scan(&watch);
            if last
                .as_ref()
                .is_some_and(|(k, run)| *k == r.per_key && *run == r.run)
            {
                continue;
            }
            last = Some((r.per_key.clone(), r.run));
            if tx.send(Event::Usage(r)).is_err() {
                return;
            }
        }
    });
    watch_tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("sulla-usage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn copilot_sums_usage_records_incrementally() {
        let dir = temp("copilot");
        let path = dir.join("events.jsonl");
        let rec = |i: u64, o: u64| {
            format!(
                "{{\"type\":\"session.usage_record\",\"data\":{{\"usage\":{{\"inputTokens\":{i},\
                 \"outputTokens\":{o},\"cacheReadTokens\":10,\"copilotUsage\":\
                 {{\"totalNanoAiu\":1500000000}}}}}}}}\n"
            )
        };
        std::fs::write(
            &path,
            format!("{}{{\"type\":\"user.message\"}}\n", rec(100, 5)),
        )
        .unwrap();
        let mut t = Transcript::default();
        t.poll(AgentKind::Copilot, || Some(path.clone()));
        assert_eq!(t.usage().input, 100);
        let partial = rec(50, 7);
        let (a, b) = partial.split_at(20);
        std::fs::write(
            &path,
            format!("{}{{\"type\":\"user.message\"}}\n{a}", rec(100, 5)),
        )
        .unwrap();
        t.poll(AgentKind::Copilot, || None);
        assert_eq!(t.usage().input, 100);
        std::fs::write(
            &path,
            format!("{}{{\"type\":\"user.message\"}}\n{a}{b}", rec(100, 5)),
        )
        .unwrap();
        t.poll(AgentKind::Copilot, || None);
        let u = t.usage();
        assert_eq!((u.input, u.output, u.cached), (150, 12, 20));
        assert!((u.credits - 3.0).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn claude_dedupes_streamed_message_chunks() {
        let dir = temp("claude");
        let path = dir.join("s.jsonl");
        let line = |id: &str, out: u64| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"id\":\"{id}\",\"usage\":{{\
                 \"input_tokens\":3,\"cache_creation_input_tokens\":10,\
                 \"cache_read_input_tokens\":100,\"output_tokens\":{out}}}}}}}\n"
            )
        };
        std::fs::write(&path, [line("a", 1), line("a", 40), line("b", 2)].concat()).unwrap();
        let mut t = Transcript::default();
        t.poll(AgentKind::Claude, || Some(path.clone()));
        let u = t.usage();
        assert_eq!((u.input, u.cached, u.output), (226, 200, 42));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_total_excludes_usage_before_first_scan() {
        let dir = temp("run");
        let sess = dir.join("session-state").join("s1");
        std::fs::create_dir_all(&sess).unwrap();
        let rec = "{\"type\":\"session.usage_record\",\"data\":{\"usage\":{\"inputTokens\":10,\"outputTokens\":1}}}\n";
        std::fs::write(sess.join("events.jsonl"), rec).unwrap();
        let mut tracker = Tracker {
            kind: AgentKind::Copilot,
            locator: Locator {
                copilot: dir.clone(),
                claude: dir.clone(),
            },
            sessions: HashMap::new(),
            baseline: HashMap::new(),
        };
        let watch = vec![("x-1".to_string(), vec!["s1".to_string(), "s2".to_string()])];
        let r = tracker.scan(&watch);
        assert_eq!(r.per_key["x-1"].input, 10);
        assert!(r.run.is_empty());
        std::fs::write(sess.join("events.jsonl"), format!("{rec}{rec}")).unwrap();
        let r = tracker.scan(&watch);
        assert_eq!(r.per_key["x-1"].input, 20);
        assert_eq!(r.run.input, 10);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn short_format() {
        let u = Usage {
            input: 1_234_567,
            output: 34_500,
            credits: 2.54,
            ..Default::default()
        };
        assert_eq!(u.short(), "1.2M in 34.5k out 2.5 cr");
    }
}
