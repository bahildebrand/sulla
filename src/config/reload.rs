use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use color_eyre::Result;

use super::Config;
use super::show::{display_path, dotted};

pub const RESTART_KEYS: [&str; 5] = [
    "sandbox",
    "incus",
    "branch_prefix",
    "base_branch",
    "workflow",
];
pub const FAILED: &str = "config reload failed: ";
pub const RESTART_SEP: &str = " · restart needed for ";
pub const FLASH: Duration = Duration::from_secs(4);
const SHOWN: usize = 6;

pub type Key = Vec<String>;
pub type Leaves = BTreeMap<Key, toml::Value>;

pub fn needs_restart(path: &[String]) -> bool {
    path.first()
        .is_some_and(|k| RESTART_KEYS.contains(&k.as_str()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: Key,
    pub old: Option<toml::Value>,
    pub new: Option<toml::Value>,
}

impl Change {
    fn label(&self) -> String {
        let name = dotted(&self.path);
        match (&self.old, &self.new) {
            (Some(a), Some(b)) if scalar(a) && scalar(b) => format!("{name} {a}→{b}"),
            _ => name,
        }
    }
}

fn scalar(v: &toml::Value) -> bool {
    matches!(
        v,
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_)
    )
}

#[derive(Debug)]
pub struct Plan {
    pub cfg: Config,
    pub changed: Vec<Change>,
    pub restart: BTreeMap<Key, Option<toml::Value>>,
}

impl Plan {
    pub fn touched(&self, top: &str) -> bool {
        self.changed
            .iter()
            .any(|c| c.path.first().is_some_and(|k| k == top))
    }

    pub fn summary(&self) -> String {
        let mut out = match self.changed.len() {
            0 => "config reloaded: no changes".to_string(),
            n => {
                let mut names: Vec<String> =
                    self.changed.iter().take(SHOWN).map(Change::label).collect();
                if n > SHOWN {
                    names.push(format!("+{} more", n - SHOWN));
                }
                format!("config reloaded: {n} changed ({})", names.join(", "))
            }
        };
        if !self.restart.is_empty() {
            let keys: Vec<String> = self.restart.keys().map(|k| dotted(k)).collect();
            out.push_str(RESTART_SEP);
            out.push_str(&keys.join(", "));
        }
        out
    }
}

pub fn leaves(cfg: &Config) -> Result<Leaves> {
    let mut out = Leaves::new();
    collect(&toml::Table::try_from(cfg)?, &mut Vec::new(), &mut out);
    Ok(out)
}

fn collect(table: &toml::Table, path: &mut Key, out: &mut Leaves) {
    for (k, v) in table {
        path.push(k.clone());
        match v {
            toml::Value::Table(t) => collect(t, path, out),
            v => {
                out.insert(path.clone(), v.clone());
            }
        }
        path.pop();
    }
}

fn set(table: &mut toml::Table, path: &[String], value: Option<toml::Value>) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut t = table;
    for p in parents {
        let entry = t
            .entry(p.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        let Some(inner) = entry.as_table_mut() else {
            return;
        };
        t = inner;
    }
    match value {
        Some(v) => {
            t.insert(last.clone(), v);
        }
        None => {
            t.remove(last);
        }
    }
}

pub fn plan(
    running: &Config,
    old: &Config,
    new: &Config,
    pending: &BTreeMap<Key, Option<toml::Value>>,
) -> Result<Plan> {
    let run = leaves(running)?;
    let was = leaves(old)?;
    let now = leaves(new)?;
    let keys: BTreeSet<&Key> = run.keys().chain(was.keys()).chain(now.keys()).collect();
    let mut table = toml::Table::try_from(new)?;
    let mut changed = Vec::new();
    let mut restart = BTreeMap::new();
    for key in keys {
        let (r, w, n) = (run.get(key), was.get(key), now.get(key));
        let overridden = r != w && !pending.contains_key(key);
        let target = if overridden && w == n { r } else { n };
        if target == r {
            set(&mut table, key, r.cloned());
        } else if needs_restart(key) {
            restart.insert(key.clone(), target.cloned());
            set(&mut table, key, r.cloned());
        } else {
            set(&mut table, key, target.cloned());
            changed.push(Change {
                path: key.clone(),
                old: r.cloned(),
                new: target.cloned(),
            });
        }
    }
    Ok(Plan {
        cfg: toml::Value::Table(table).try_into()?,
        changed,
        restart,
    })
}

pub fn describe(e: &color_eyre::Report) -> String {
    let file = e
        .chain()
        .next()
        .map(|c| display_path(Path::new(&c.to_string())));
    let toml = e.chain().find_map(|c| c.downcast_ref::<toml::de::Error>());
    match (file, toml) {
        (Some(file), Some(t)) => {
            let text = t.to_string();
            let at = text
                .lines()
                .next()
                .and_then(|l| l.split_once(" at "))
                .map(|(_, at)| format!(" {at}"))
                .unwrap_or_default();
            format!("{file}{at}: {}", t.message().trim())
        }
        _ => format!("{e:#}")
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
    }
}

#[derive(Debug, Default, Clone)]
pub struct State {
    pub error: Option<String>,
    pub restart: BTreeMap<Key, Option<toml::Value>>,
    pub changed: BTreeSet<Key>,
    pub at: Option<Instant>,
}

impl State {
    pub fn flashing(&self, path: &[String], now: Instant) -> bool {
        self.at.is_some_and(|at| now.duration_since(at) < FLASH) && self.changed.contains(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: m.modified().ok(),
        len: m.len(),
    })
}

#[derive(Debug)]
pub struct Watch {
    files: Vec<PathBuf>,
    seen: Vec<Option<Stamp>>,
    dirty: Option<Instant>,
    polled: Option<Instant>,
}

impl Watch {
    pub const EVERY: Duration = Duration::from_millis(250);
    pub const SETTLE: Duration = Duration::from_millis(300);

    pub fn new(files: Vec<PathBuf>) -> Self {
        let seen = files.iter().map(|f| stamp(f)).collect();
        Self {
            files,
            seen,
            dirty: None,
            polled: None,
        }
    }

    pub fn sync(&mut self) {
        self.seen = self.files.iter().map(|f| stamp(f)).collect();
        self.dirty = None;
    }

    pub fn poll(&mut self, now: Instant) -> bool {
        if self
            .polled
            .is_some_and(|p| now.saturating_duration_since(p) < Self::EVERY)
        {
            return false;
        }
        self.polled = Some(now);
        let cur: Vec<Option<Stamp>> = self.files.iter().map(|f| stamp(f)).collect();
        if cur != self.seen {
            self.seen = cur;
            self.dirty = Some(now);
            return false;
        }
        match self.dirty {
            Some(t) if now.saturating_duration_since(t) >= Self::SETTLE => {
                self.dirty = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    fn key(s: &str) -> Key {
        s.split('.').map(String::from).collect()
    }

    fn names(p: &Plan) -> Vec<String> {
        p.changed.iter().map(|c| dotted(&c.path)).collect()
    }

    #[test]
    fn restart_keys_cover_whole_tables() {
        assert!(needs_restart(&key("sandbox")));
        assert!(needs_restart(&key("incus.memory")));
        assert!(needs_restart(&key("incus.browse.ports")));
        assert!(needs_restart(&key("branch_prefix")));
        assert!(needs_restart(&key("base_branch")));
        assert!(needs_restart(&key("workflow")));
        assert!(!needs_restart(&key("max_agents")));
        assert!(!needs_restart(&key("claude.model")));
        assert!(!needs_restart(&key("pr.poll_secs")));
        assert!(!needs_restart(&key("incusx")));
    }

    #[test]
    fn live_keys_apply_and_restart_keys_stay_pending() {
        let old = cfg("max_agents = 3");
        let new = cfg(
            "max_agents = 4\nsandbox = \"incus\"\n[claude]\nmodel = \"opus\"\n[incus]\nmemory = \"4GiB\"\n[notify]\nmethods = [\"desktop\"]",
        );
        let p = plan(&old, &old, &new, &BTreeMap::new()).unwrap();
        assert_eq!(p.cfg.max_agents, 4);
        assert_eq!(p.cfg.claude.model.as_deref(), Some("opus"));
        assert_eq!(p.cfg.notify.methods, new.notify.methods);
        assert_eq!(p.cfg.sandbox, old.sandbox);
        assert_eq!(p.cfg.incus.memory, "8GiB");
        assert_eq!(names(&p), ["claude.model", "max_agents", "notify.methods"]);
        assert_eq!(
            p.restart.keys().cloned().collect::<Vec<_>>(),
            [key("incus.memory"), key("sandbox")]
        );
        assert_eq!(
            p.restart[&key("incus.memory")],
            Some(toml::Value::String("4GiB".into()))
        );
        assert!(p.touched("notify"));
        assert!(!p.touched("pr"));
        assert_eq!(
            p.summary(),
            "config reloaded: 3 changed (claude.model, max_agents 3→4, notify.methods) · restart needed for incus.memory, sandbox"
        );
    }

    #[test]
    fn nothing_changed_reports_no_changes() {
        let c = cfg("max_agents = 5\n[presets]\nrust = \"r\"");
        let p = plan(&c, &c, &c, &BTreeMap::new()).unwrap();
        assert!(p.changed.is_empty() && p.restart.is_empty());
        assert_eq!(p.summary(), "config reloaded: no changes");
    }

    #[test]
    fn unset_options_and_new_table_entries_diff_per_key() {
        let old = cfg("[claude]\nmodel = \"opus\"");
        let new = cfg("[presets]\nrust = \"r\"\n[incus.env]\nA = \"1\"");
        let p = plan(&old, &old, &new, &BTreeMap::new()).unwrap();
        assert_eq!(names(&p), ["claude.model", "presets.rust"]);
        assert_eq!(p.changed[0].new, None);
        assert_eq!(p.cfg.claude.model, None);
        assert_eq!(p.cfg.presets["rust"], "r");
        assert!(p.cfg.incus.env.is_empty());
        assert_eq!(
            p.restart.keys().cloned().collect::<Vec<_>>(),
            [key("incus.env.A")]
        );
        assert_eq!(
            p.summary(),
            "config reloaded: 2 changed (claude.model, presets.rust) · restart needed for incus.env.A"
        );
    }

    #[test]
    fn runtime_overrides_survive_unless_their_file_value_changed() {
        let file = cfg("max_agents = 3\npoll_secs = 2");
        let mut running = file.clone();
        running.max_agents = 8;
        running.stall.minutes = 9;
        let new = cfg("max_agents = 4\npoll_secs = 5");
        let p = plan(&running, &file, &new, &BTreeMap::new()).unwrap();
        assert_eq!(p.cfg.max_agents, 4);
        assert_eq!(p.cfg.stall.minutes, 9);
        assert_eq!(p.cfg.poll_secs, 5);
        assert_eq!(names(&p), ["max_agents", "poll_secs"]);
        assert_eq!(p.changed[0].old, Some(toml::Value::Integer(8)));

        let p = plan(&running, &file, &file, &BTreeMap::new()).unwrap();
        assert_eq!(p.cfg.max_agents, 8);
        assert_eq!(p.cfg.stall.minutes, 9);
        assert!(p.changed.is_empty());
    }

    #[test]
    fn pending_restart_clears_when_the_file_reverts() {
        let running = cfg("");
        let edited = cfg("sandbox = \"incus\"");
        let p = plan(&running, &running, &edited, &BTreeMap::new()).unwrap();
        assert_eq!(p.restart.len(), 1);
        let again = plan(&p.cfg, &edited, &edited, &p.restart).unwrap();
        assert_eq!(again.restart, p.restart);
        assert!(again.changed.is_empty());
        let reverted = plan(&p.cfg, &edited, &running, &p.restart).unwrap();
        assert!(reverted.restart.is_empty());
        assert!(reverted.changed.is_empty());
        assert_eq!(reverted.cfg.sandbox, running.sandbox);
    }

    #[test]
    fn long_change_lists_are_capped() {
        let old = cfg("");
        let new = cfg(
            "max_agents = 9\npoll_secs = 9\ndone_retention_days = 9\nkache = false\ncheck = \"x\"\nsystem_prompt = \"y\"\nagent = \"claude\"",
        );
        let p = plan(&old, &old, &new, &BTreeMap::new()).unwrap();
        assert_eq!(p.changed.len(), 7);
        assert!(p.summary().ends_with(", +1 more)"), "{}", p.summary());
        assert!(p.summary().contains("kache true→false"), "{}", p.summary());
    }

    #[test]
    fn describe_names_file_line_and_message() {
        let dir = std::env::temp_dir().join(format!("sulla-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "poll_secs = 1\nmax_agents = \"many\"\n").unwrap();
        let e = super::super::effective::Layer::read(&file).unwrap_err();
        let msg = describe(&e);
        assert!(msg.starts_with(&display_path(&file)), "{msg}");
        assert!(msg.contains("line 2, column 14"), "{msg}");
        assert!(msg.contains("invalid type"), "{msg}");
        assert!(!msg.contains('\n'), "{msg}");
        std::fs::write(&file, "[claude\n").unwrap();
        let msg = describe(&super::super::effective::Layer::read(&file).unwrap_err());
        assert!(msg.contains("line 1"), "{msg}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn watch_waits_for_the_file_to_settle() {
        let dir = std::env::temp_dir().join(format!("sulla-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        let mut w = Watch::new(vec![dir.join("none.toml"), file.clone()]);
        let t0 = Instant::now();
        assert!(!w.poll(t0));
        std::fs::write(&file, "max_agents = 4\n").unwrap();
        assert!(!w.poll(t0 + Duration::from_millis(100)));
        assert!(!w.poll(t0 + Duration::from_millis(300)));
        assert!(!w.poll(t0 + Duration::from_millis(400)));
        assert!(w.poll(t0 + Duration::from_millis(700)));
        assert!(!w.poll(t0 + Duration::from_millis(1000)));
        std::fs::write(&file, "max_agents = 40\n").unwrap();
        assert!(!w.poll(t0 + Duration::from_millis(1300)));
        w.sync();
        assert!(!w.poll(t0 + Duration::from_millis(1700)));
        std::fs::remove_file(&file).unwrap();
        assert!(!w.poll(t0 + Duration::from_millis(2000)));
        assert!(w.poll(t0 + Duration::from_millis(2400)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
